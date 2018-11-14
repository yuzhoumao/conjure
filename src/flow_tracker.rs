use std::collections::{HashMap, VecDeque};
use time::precise_time_ns;

use std::net::IpAddr;
use pnet::packet::tcp::TcpPacket;

use util::IpPacket;
use std::fmt;

// All members are stored in host-order, even src_ip and dst_ip.
#[derive(PartialEq,Eq,Hash,Copy,Clone,Debug)]
pub struct Flow
{
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub src_port: u16,
    pub dst_port: u16,
}


impl fmt::Display for Flow {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{} -> {}:{}", self.src_ip, self.src_port, self.dst_ip, self.dst_port)
    }
}

impl Flow
{
    pub fn new(ip_pkt: &IpPacket, tcp_pkt: &TcpPacket) -> Flow
    {
        match ip_pkt {
            IpPacket::V4(pkt) => Flow {
                src_ip: IpAddr::V4(pkt.get_source()),
                dst_ip: IpAddr::V4(pkt.get_destination()),
                src_port: tcp_pkt.get_source(),
                dst_port: tcp_pkt.get_destination()
            },
            IpPacket::V6(pkt) => Flow {
                src_ip: IpAddr::V6(pkt.get_source()),
                dst_ip: IpAddr::V6(pkt.get_destination()),
                src_port: tcp_pkt.get_source(),
                dst_port: tcp_pkt.get_destination()
            },
        }
    }
    pub fn from_parts(sip: IpAddr, dip: IpAddr, sport: u16, dport: u16) -> Flow
    {
        Flow { src_ip: sip, dst_ip: dip, src_port: sport, dst_port: dport }
    }
}
#[derive(Copy,Clone)]
enum FlowState
{
    InTLSHandshake,     // After SYN, before first app packet (might signal us)
    ActiveTag(u64),     // Upon a signal, we create the specified flow
                        // client -> client-specified dark decoy
                        // and tag it with this.
                        // The u64 is the time (ns) that this times out.
}

pub struct SchedEvent
{
    // Nanoseconds since an unspecified epoch (precise_time_ns()).
    drop_time: u64,
    flow: Flow,
}

pub struct FlowTracker
{
    // Keys present in this map are flows we have any interest in.
    // Key not present in map => sure flow isn't of interest. Ignore all non-SYN packets.
    // Key present, value InTLSHandshake => don't yet know if it's of interest yet
    tracked_flows:  HashMap<Flow, FlowState>,
    stale_drops:    VecDeque<SchedEvent>,
}

// Amount of time that we timeout all flows
const TIMEOUT_NS: u64 = 30*1000*1000*1000;
//const FIN_TIMEOUT_NS: u64 = 2*1000*1000*1000;

impl FlowTracker
{
    pub fn new() -> FlowTracker
    {
        FlowTracker
        {
            tracked_flows: HashMap::new(),
            stale_drops: VecDeque::with_capacity(16384),
        }
    }
    pub fn begin_tracking_flow(&mut self, flow: &Flow)
    {
        // Always push back, even if the entry was already there. Doesn't hurt
        // to do a second check on overdueness, and this is simplest.
        self.stale_drops.push_back(
            SchedEvent { drop_time: precise_time_ns() + TIMEOUT_NS,
                         flow: *flow });
        // Begin tracking as a potential TD flow (if not already in the map).
        self.tracked_flows.entry(*flow)
                          .or_insert(FlowState::InTLSHandshake);
    }
    pub fn is_tagged(&self, flow: &Flow) -> bool
    {
        match self.tracked_flows.get(&flow) {
            None => false,
            Some(to_check) => match *to_check {
                FlowState::InTLSHandshake   => false,
                FlowState::ActiveTag(_)     => true,
            },
        }
    }
    /*
    pub fn forward_syn(&self, flow: &Flow, zmq_id: u8)
    {
        if let Some(to_check) = self.tracked_flows.get(&flow) {
            if let FlowState::InTLSHandshake(ref pkt) = *to_check {
                c_api::c_send_packet_to_proxy(zmq_id, &pkt);
            }
        }
    }*/

    pub fn tracking_at_all(&self, flow: &Flow) -> bool
    {
        self.tracked_flows.contains_key(flow)
    }
    // Set this flow tagged
    pub fn mark_tagged(&mut self, flow: &Flow)
    {
        let expire_time = precise_time_ns() + TIMEOUT_NS;
        self.stale_drops.push_back(
            SchedEvent { drop_time: expire_time,
                         flow: *flow});

        let val = FlowState::ActiveTag(expire_time);

        *self.tracked_flows.entry(*flow).or_insert(val) = val;
    }

    pub fn drop(&mut self, flow: &Flow)
    {
        if self.is_tagged(flow) {
            debug!("delflow {}:{} -> {}:{}",
                    flow.src_ip, flow.src_port, flow.dst_ip, flow.dst_port);
        }
        self.tracked_flows.remove(flow);
    }

    fn process_scheduled_drop(&mut self, flow: &Flow, right_now: u64)
    {
        let do_drop = {
            if let Some(val) = self.tracked_flows.get(flow) {
                match *val {
                    FlowState::InTLSHandshake => true,
                    FlowState::ActiveTag(drop_time) => (right_now > drop_time),
                    // Don't timeout active tapdance flows
                }
            }
            else {false}
        };
        if do_drop {
            self.drop(flow);
        }
    }
    // This function returns the number of flows that it drops.
    #[allow(non_snake_case)]
    pub fn drop_stale_flows(&mut self) -> usize
    {
        let right_now = precise_time_ns();
        let num_flows_before = self.tracked_flows.len();
        while !self.stale_drops.is_empty() && // is_empty: condition for unwraps
               self.stale_drops.front().unwrap().drop_time <= right_now
        {
            let cur = self.stale_drops.pop_front().unwrap();
            self.process_scheduled_drop(&cur.flow, right_now);
        }
        let num_flows_after = self.tracked_flows.len();

        //debug!("dropped {} stale flows, now tracking {} flows",
        //         num_flows_before - num_flows_after,
        //         num_flows_after);
        num_flows_before - num_flows_after
    }

    pub fn count_tracked_flows(&self) -> usize
    {
        self.tracked_flows.len()
    }
}




















// TODO get these into their own file

#[cfg(test)]
mod tests {
#![allow(non_upper_case_globals)]
use std::thread::sleep;
use std::time;

use flow_tracker::{Flow,FlowTracker,FIN_TIMEOUT_NS,WscaleAndMSS};

const flow1: Flow =
    Flow { src_ip: 1234, dst_ip: 5678, src_port: 33333, dst_port: 443 };
const flow1_seq: u32 = 111;
const flow2: Flow =
    Flow { src_ip: 4321, dst_ip: 8765, src_port: 44444, dst_port: 80 };
const flow2_seq: u32 = 222;
const flow3: Flow =
    Flow { src_ip: 4321, dst_ip: 8765, src_port: 44444, dst_port: 22 };
const flow3_seq: u32 = 333;

const flow1_clone: Flow =
    Flow { src_ip: 1234, dst_ip: 5678, src_port: 33333, dst_port: 443 };
const flow1_diff_srcip: Flow =
    Flow { src_ip: 999, dst_ip: 5678, src_port: 33333, dst_port: 443 };
const flow1_diff_dstip: Flow =
    Flow { src_ip: 1234, dst_ip: 999, src_port: 33333, dst_port: 443 };
const flow1_diff_sport: Flow =
    Flow { src_ip: 1234, dst_ip: 5678, src_port: 55555, dst_port: 443 };
const flow1_diff_dport: Flow =
    Flow { src_ip: 1234, dst_ip: 5678, src_port: 33333, dst_port: 80 };

fn test_default_syn() -> Vec<u8>
{
    vec!(0xe3, 0x2c, // src port
         0x01, 0xbb, // dst port 443
         0x43, 0xb0, 0x9f, 0x78, // seq# (1135648632)
         0, 0, 0, 0, // ACK 0
         160, // 50 byte header = offset 10, 10 << 4 = 160
         2, // SYN flag
         0xaa, 0xaa, // window
         0x5a, 0x0e, // checksum
         0, 0, // urgent pointer
         // 20 bytes of options, from the SYN of `iperf -c localhost -p 443`:
         // [mss 65495,sackOK,TS val 885507 ecr 0,nop,wscale 7]
         0x02, 0x04, 0xff, 0xd7, // mss 65495
         0x04, 0x02, 0x08, 0x0a, 0x00, 0x0d, 0x83, 0x03,
         0x00, 0x00, 0x00, 0x00, 0x01, 0x03, 0x03, 0x07)
}

#[test]
fn begin_tracking_flow_add_flows()
{
    let mut ft = FlowTracker::new();
    assert_eq!(0, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1, test_default_syn());
    assert_eq!(1, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow2, test_default_syn());
    assert_eq!(2, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow3, test_default_syn());
    assert_eq!(3, ft.tracked_flows.len());
}

#[test]
fn begin_tracking_uses_whole_4tuple()
{
    let mut ft = FlowTracker::new();
    assert_eq!(0, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1, test_default_syn());
    assert_eq!(1, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1_diff_srcip, test_default_syn());
    assert_eq!(2, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1_diff_dstip, test_default_syn());
    assert_eq!(3, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1_diff_sport, test_default_syn());
    assert_eq!(4, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1_diff_dport, test_default_syn());
    assert_eq!(5, ft.tracked_flows.len());
}

#[test]
fn flow_equality_uses_whole_4tuple()
{
    assert_eq!(flow1, flow1_clone);
    assert!(flow1 != flow1_diff_srcip);
    assert!(flow1 != flow1_diff_dstip);
    assert!(flow1 != flow1_diff_sport);
    assert!(flow1 != flow1_diff_dport);
}

#[test]
fn begin_tracking_flow_ignore_duplicate()
{
    let mut ft = FlowTracker::new();
    assert_eq!(0, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1, test_default_syn());
    assert_eq!(1, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow2, test_default_syn());
    assert_eq!(2, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1, test_default_syn());
    assert_eq!(2, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1_clone, test_default_syn());
    assert_eq!(2, ft.tracked_flows.len());
}

#[test]
fn mark_yes_and_query_flow_status()
{
    let mut ft = FlowTracker::new();
    ft.begin_tracking_flow(&flow1, test_default_syn());
    ft.begin_tracking_flow(&flow2, test_default_syn());
    assert!(!ft.is_td(&flow1));
    assert!(!ft.is_td(&flow2));
    assert!(!ft.is_td(&flow3));
    assert!(ft.tracking_at_all(&flow1));
    assert!(ft.tracking_at_all(&flow2));
    assert!(!ft.tracking_at_all(&flow3));
    ft.mark_tapdance_flow(&flow1, flow1_seq, 1);
    assert!(ft.is_td(&flow1));
    assert!(!ft.is_td(&flow2));
    ft.mark_tapdance_flow(&flow2, flow2_seq, 1);
    assert!(ft.is_td(&flow1));
    assert!(ft.is_td(&flow2));
    ft.begin_tracking_flow(&flow3, test_default_syn());
    assert!(ft.tracking_at_all(&flow3));
}

// Well, panic isn't really the right behavior for this error. Unfortunately
// Rust's testing doesn't allow you to expect an error!(), just a panic!().
// If you want to run this test, change the error!() in mark_tapdance_flow()
// back to a panic!().
// #[test]
// #[should_panic]
// fn mark_yes_nonexistant_panics()
// {
//     let mut ft = FlowTracker::new();
//     ft.begin_tracking_flow(&flow1, test_default_syn());
//     ft.mark_tapdance_flow(&flow2, flow2_seq);
// }

#[test]
fn drop()
{
    let mut ft = FlowTracker::new();
    ft.drop(&flow3);
    assert_eq!(0, ft.tracked_flows.len());
    ft.begin_tracking_flow(&flow1, test_default_syn());
    ft.begin_tracking_flow(&flow2, test_default_syn());
    assert_eq!(2, ft.tracked_flows.len());
    ft.drop(&flow1);
    assert_eq!(1, ft.tracked_flows.len());
    ft.drop(&flow1);
    assert_eq!(1, ft.tracked_flows.len());
    ft.drop(&flow2);
    assert_eq!(0, ft.tracked_flows.len());
    ft.drop(&flow3);
    assert_eq!(0, ft.tracked_flows.len());
}

#[test]
fn drop_stale_flows_empty_no_panic()
{
    let mut ft = FlowTracker::new();
    ft.drop_stale_flows_and_RST_FINd();
}

#[test]
#[ignore]
fn drop_stale_flows()
{
    let mut ft = FlowTracker::new();
    ft.begin_tracking_flow(&flow1, test_default_syn());
    ft.begin_tracking_flow(&flow2, test_default_syn());
    sleep(time::Duration::from_millis(1000));
    ft.drop_stale_flows_and_RST_FINd();
    assert_eq!(2, ft.tracked_flows.len());
    ft.mark_tapdance_flow(&flow1, flow1_seq, 1);
    sleep(time::Duration::from_millis(2000));
    ft.begin_tracking_flow(&flow3, test_default_syn());
    assert_eq!(3, ft.tracked_flows.len());
    sleep(time::Duration::from_millis(5500));
    ft.drop_stale_flows_and_RST_FINd();
    assert!(ft.is_td(&flow1));
    assert!(!ft.tracking_at_all(&flow2));
    assert!(ft.tracking_at_all(&flow3));
}

#[test]
#[ignore]
fn drop_stale_does_not_drop_fin()
{
    let mut ft = FlowTracker::new();
    ft.begin_tracking_flow(&flow1, test_default_syn());
    ft.mark_tapdance_flow(&flow1, flow1_seq, 1);
    sleep(time::Duration::from_millis(7500));
    ft.notice_fin(&flow1);
    sleep(time::Duration::from_millis(510));
    ft.drop_stale_flows_and_RST_FINd();
    assert!(ft.is_td(&flow1));
}

#[test]
fn finishing_td_is_still_td()
{
    let mut ft = FlowTracker::new();
    ft.begin_tracking_flow(&flow1, test_default_syn());
    ft.mark_tapdance_flow(&flow1, flow1_seq, 1);
    ft.notice_fin(&flow1);
    assert!(ft.is_td(&flow1));
}

// Potential regression that this test checks for (if you know to look for it):
// quicker RST events getting head-of-line blocked by the slower stale-drop
// ones. If it's failing unless you set the sleep dur to > the stale drop wait,
// your clock isn't broken, you just have the head-of-line blocking problem!
// HACK: the should_panic is a very hacky mock expectation
//
// Marked "_VERY_IMPORTANT_MUST_PASS" because if our hacky mock system got
// messed up, then no_tapdance_no_rst might erroneously pass, and so long as
// this test is passing, you can be sure that isn't the case.
#[test]
#[should_panic(expected = "c_tcp_send_rst_pkt(111) called")]
fn rst_2_seconds_after_fin_VERY_IMPORTANT_MUST_PASS()
{
    let mut ft = FlowTracker::new();
    ft.begin_tracking_flow(&flow1, test_default_syn());
    ft.mark_tapdance_flow(&flow1, flow1_seq, 1);
    ft.notice_fin(&flow1);
    assert!(ft.is_td(&flow1));
    sleep(time::Duration::from_millis(FIN_TIMEOUT_NS/1000000 + 50));
    ft.drop_stale_flows_and_RST_FINd();
}

// THIS IS A VERY IMPORTANT TEST. If c_tcp_send_rst_pkt() gets called, then this
// version of the station WOULD RST EVERY NON-TAPDANCE HTTPS FLOW!!!!!!!!!!
// HACK: the (lack of) should_panic is a very hacky mock expectation
#[test]
fn no_tapdance_no_rst_VERY_IMPORTANT_MUST_PASS()
{
    let mut ft = FlowTracker::new();
    ft.begin_tracking_flow(&flow1, test_default_syn());
    ft.begin_tracking_flow(&flow2, test_default_syn());
    ft.notice_fin(&flow1);
    sleep(time::Duration::from_millis(FIN_TIMEOUT_NS/1000000 + 50));
    ft.drop_stale_flows_and_RST_FINd();
}

#[test]
fn mss_and_wscale_remembered()
{
    let mut ft = FlowTracker::new();
    ft.begin_tracking_flow(&flow1, test_default_syn());
    assert!(!ft.is_td(&flow1));
    assert!(ft.tracking_at_all(&flow1));
    let wscale_and_mss = ft.mark_tapdance_flow(&flow1, flow1_seq, 1);
    assert_eq!(65495, wscale_and_mss.mss);
    assert_eq!(7, wscale_and_mss.wscale);
}

#[test]
fn count_tracked_flows_counts()
{
    let mut ft = FlowTracker::new();
    assert_eq!(0, ft.count_tracked_flows());
    ft.drop(&flow3);
    assert_eq!(0, ft.count_tracked_flows());
    ft.begin_tracking_flow(&flow1, test_default_syn());
    assert_eq!(1, ft.count_tracked_flows());
    ft.begin_tracking_flow(&flow2, test_default_syn());
    ft.begin_tracking_flow(&flow3, test_default_syn());
    assert_eq!(3, ft.count_tracked_flows());
    ft.drop(&flow1);
    assert_eq!(2, ft.count_tracked_flows());
    ft.drop(&flow1);
    assert_eq!(2, ft.count_tracked_flows());
    ft.drop(&flow2);
    assert_eq!(1, ft.count_tracked_flows());
    ft.drop(&flow3);
    assert_eq!(0, ft.count_tracked_flows());
}

// TODO passive tests

} // mod tests

