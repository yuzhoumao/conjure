package main

import (
	"encoding/hex"
	"fmt"
	"io/ioutil"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync"
	"syscall"

	"github.com/BurntSushi/toml"
	"github.com/golang/protobuf/proto"
	"github.com/gorilla/mux"
	zmq "github.com/pebbe/zmq4"
	pb "github.com/refraction-networking/gotapdance/protobuf"
)

const (
	// The length of the shared secret sent by the client in bytes.
	regIDLen     = 16
	SecretLength = 32
)

type config struct {
	APIPort           uint16   `toml:"api_port"`
	ZMQPort           uint16   `toml:"zmq_port"`
	PrivateKeyPath    string   `toml:"privkey_path"`
	AuthType          string   `toml:"auth_type"`
	AuthVerbose       bool     `toml:"auth_verbose"`
	StationPublicKeys []string `toml:"station_pubkeys"`

	// Parsed from conjure.conf environment vars
	logClientIP bool
}

type server struct {
	sync.Mutex
	config

	// Function to accept message into processing queue. Abstracted
	// to allow mocking of ZMQ send flow
	messageAccepter func([]byte) error

	logger *log.Logger
	sock   *zmq.Socket
}

// Get the first element of the X-Forwarded-For header if it is available, this
// will be the clients address if intermediate proxies follow X-Forwarded-For
// specification (as seen here: https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/X-Forwarded-For).
// Otherwise return the remote address specified in the request.
//
// In the future this may need to handle True-Client-IP headers.
func getRemoteAddr(r *http.Request) string {
	if r.Header.Get("X-Forwarded-For") != "" {
		addrList := r.Header.Get("X-Forwarded-For")
		return strings.Trim(strings.Split(addrList, ",")[0], " \t")
	}
	return r.RemoteAddr
}

func (s *server) register(w http.ResponseWriter, r *http.Request) {
	requestIP := getRemoteAddr(r)

	if s.logClientIP {
		s.logger.Printf("received %s request from IP %v with content-length %d\n", r.Method, requestIP, r.ContentLength)
	} else {
		s.logger.Printf("received %s request from IP _ with content-length %d\n", r.Method, r.ContentLength)
	}

	const MinimumRequestLength = SecretLength + 1 // shared_secret + VSP
	if r.Method != "POST" {
		s.logger.Printf("rejecting request due to incorrect method %s\n", r.Method)
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}

	if r.ContentLength < MinimumRequestLength {
		s.logger.Printf("rejecting request due to short content-length of %d, expecting at least %d\n", r.ContentLength, MinimumRequestLength)
		http.Error(w, "Payload too small", http.StatusBadRequest)
		return
	}

	in, err := ioutil.ReadAll(r.Body)
	if err != nil {
		s.logger.Println("failed to read request body:", err)
		http.Error(w, "Failed to read request body", http.StatusBadRequest)
		return
	}

	payload := &pb.C2SWrapper{}
	if err = proto.Unmarshal(in, payload); err != nil {
		s.logger.Println("failed to decode protobuf body:", err)
		http.Error(w, "Failed to decode protobuf body", http.StatusBadRequest)
		return
	}

	clientAddr := parseIP(requestIP)
	var clientAddrBytes = make([]byte, 16, 16)
	if clientAddr != nil {
		clientAddrBytes = []byte(clientAddr.To16())
	}

	zmqPayload, err := s.processC2SWrapper(payload, clientAddrBytes)
	if err != nil {
		s.logger.Println("failed to marshal ClientToStation into VSP:", err)
		w.WriteHeader(http.StatusInternalServerError)
		return
	}

	err = s.messageAccepter(zmqPayload)
	if err != nil {
		s.logger.Println("failed to publish registration:", err)
		w.WriteHeader(http.StatusInternalServerError)
		return
	}

	// We could send an HTTP response earlier to avoid waiting
	// while the zmq socket is locked, but this ensures that
	// a 204 truly indicates registration success.
	w.WriteHeader(http.StatusNoContent)
}

func (s *server) sendToZMQ(message []byte) error {
	s.Lock()
	_, err := s.sock.SendBytes(message, zmq.DONTWAIT)
	s.Unlock()

	return err
}

func (s *server) processC2SWrapper(clientToAPIProto *pb.C2SWrapper, clientAddr []byte) ([]byte, error) {
	payload := &pb.C2SWrapper{}

	if clientToAPIProto == nil {
		return nil, fmt.Errorf("unable to process nil C2SWrapper")
	}

	if len(clientToAPIProto.GetSharedSecret()) < regIDLen/2 {
		return nil, fmt.Errorf("shared secret undefined or insufficient length")
	}

	// If the channel that the registration was received over was not specified
	// in the C2SWrapper set it here as API.
	if clientToAPIProto.GetRegistrationSource() == pb.RegistrationSource_Unspecified {
		source := pb.RegistrationSource_API
		payload.RegistrationSource = &source
	} else {
		source := clientToAPIProto.GetRegistrationSource()
		payload.RegistrationSource = &source
	}

	// If the address that the registration was received from was NOT set in the
	// C2SWrapper set it here to the source address of the API request.
	if clientToAPIProto.GetRegistrationAddress() == nil ||
		clientToAPIProto.GetRegistrationSource() == pb.RegistrationSource_API {
		payload.RegistrationAddress = clientAddr
	} else {
		payload.RegistrationAddress = clientToAPIProto.GetRegistrationAddress()
	}

	payload.SharedSecret = clientToAPIProto.GetSharedSecret()
	payload.RegistrationPayload = clientToAPIProto.GetRegistrationPayload()

	s.logger.Printf("forwarding registration %s source %v \n", hex.EncodeToString(payload.GetSharedSecret())[:regIDLen], payload.GetRegistrationSource())
	return proto.Marshal(payload)
}

// setupReloadHandler spawns a lightweight thread to listen for reload signals
// and loads updated configurations for the registration-api process everytime
// the reload signal is received
func (s *server) setupReloadHandler() {
	signalChan := make(chan os.Signal, 1)

	signal.Notify(
		signalChan,
		syscall.SIGHUP, // listen for SIGHUP as reload signal
	)

	// spawn a goroutine to handle os signals continuously
	go func() {
		for {
			<-signalChan
			s.loadNewConfig()
		}
	}()
}

// loadNewConfig reads configuration for registration-api, and updates all
// in-memory configs other than the ports. Updating the port of ZMQ socket
// and/or the port of the application should require a restart.
func (s *server) loadNewConfig() {
	s.Lock()
	defer s.Unlock()

	s.logger.Printf("reloading config for registration API")

	_, err := toml.DecodeFile(os.Getenv("CJ_API_CONFIG"), &s)
	if err != nil {
		s.logger.Fatalln("failed to load config:", err)
	}

	// AuthStart() is not idempotent, must explictly stop auth before updating curve
	zmq.AuthStop()
	// update the auth curve of the ZMQ socket without creating a new one
	s.setupAuth(s.sock)
}

// setupAuth resets the auth settings based on the configuration
func (s *server) setupAuth(sock *zmq.Socket) {
	if s.AuthType == "CURVE" {
		// always read from key path everytime this function is called because
		// even if the key path stays the same, the key content may have changed
		privkeyBytes, err := ioutil.ReadFile(s.PrivateKeyPath)
		if err != nil {
			s.logger.Fatalln("failed to get private key:", err)
		}

		privkey := zmq.Z85encode(string(privkeyBytes[:32]))

		zmq.AuthSetVerbose(s.AuthVerbose)

		err = zmq.AuthStart()
		if err != nil {
			s.logger.Fatalln("failed to start zmq auth:", err)
		}

		s.logger.Println(s.StationPublicKeys)
		zmq.AuthAllow("*")
		zmq.AuthCurveAdd("*", s.StationPublicKeys...)

		err = sock.ServerAuthCurve("*", privkey)
		if err != nil {
			s.logger.Fatalln("failed to set up auth on zmq socket:", err)
		}
	}
}

// parseIP attempts to parse the IP address of a request from string format wether
// it has a port attached to it or not. Returns nil if parse fails.
func parseIP(addrPort string) *net.IP {

	// by default format from r.RemoteAddr is host:port
	host, _, err := net.SplitHostPort(addrPort)
	if err != nil || host == "" {
		// if the request ends up as host only this should catch it.
		addr := net.ParseIP(addrPort)
		if addr == nil {
			return nil
		}
		return &addr
	}

	addr := net.ParseIP(host)

	return &addr

}

func main() {
	var s server
	s.logger = log.New(os.Stdout, "[API] ", log.Ldate|log.Lmicroseconds)
	s.messageAccepter = s.sendToZMQ

	_, err := toml.DecodeFile(os.Getenv("CJ_API_CONFIG"), &s)
	if err != nil {
		s.logger.Fatalln("failed to load config:", err)
	}

	// Should we log client IP addresses
	s.logClientIP, err = strconv.ParseBool(os.Getenv("LOG_CLIENT_IP"))
	if err != nil {
		s.logger.Printf("failed parse client ip logging setting: %v\n", err)
		s.logClientIP = false
	}

	sock, err := zmq.NewSocket(zmq.PUB)
	if err != nil {
		s.logger.Fatalln("failed to create zmq socket:", err)
	}

	s.setupAuth(sock)

	err = sock.Bind(fmt.Sprintf("tcp://*:%d", s.ZMQPort))
	if err != nil {
		s.logger.Fatalln("failed to bind zmq socket:", err)
	}
	s.sock = sock

	s.logger.Println("bound zmq socket")

	s.logger.Printf("starting HTTP API on port %d\n", s.APIPort)

	r := mux.NewRouter()
	r.HandleFunc("/register", s.register)
	http.Handle("/", r)

	s.setupReloadHandler()

	s.logger.Fatal(http.ListenAndServe(fmt.Sprintf(":%d", s.APIPort), nil))
}
