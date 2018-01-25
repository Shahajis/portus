extern crate clap;

#[macro_use]
extern crate slog;
#[macro_use]
extern crate portus;

use std::vec::Vec;
use std::collections::HashMap;
use portus::{Aggregator, CongAlg, Config, Datapath, DatapathInfo, Measurement};
use portus::pattern;
use portus::ipc::Ipc;
use portus::lang::Scope;

pub struct AggregationExample<T: Ipc> {
    logger: Option<slog::Logger>,
    sc: Option<Scope>,
    cwnd: u32,
    curr_cwnd_reduction: u32,
    init_cwnd: u32,
    ss_thresh: u32,
    subflow: HashMap<u32, Datapath<T> >,
    subflow_pending: HashMap<u32, u32>,
    subflow_rtt: HashMap<u32, u32>,
    subflow_cwnd: HashMap<u32, u32>,
    subflow_util: HashMap<u32, u32>,
    subflow_inflight: HashMap<u32, u32>,
    num_flows: u32,
}

pub const DEFAULT_SS_THRESH: u32 = 0x7fffffff;
pub const DEFAULT_PENDING_BYTES: u32 = 14480;

#[derive(Clone)]
pub struct AggregationExampleConfig {}

impl Default for AggregationExampleConfig {
    fn default() -> Self {
        AggregationExampleConfig{}
    }
}

#[derive(PartialEq, Eq, Hash)]
#[derive(Clone, Copy)]
pub struct AggregationExampleKey(u32);

impl From<DatapathInfo> for AggregationExampleKey {
    /// Aggregate all flows from this IP.
    /// A more complete implementation might also consider the destination IP in its heuristic
    /// for whether the flows share a bottleneck.
    fn from(d: DatapathInfo) -> Self {
        AggregationExampleKey(d.src_ip)
    }
}

impl<T: Ipc> Aggregator<T> for AggregationExample<T> {
    type Key = AggregationExampleKey;
    
    fn new_flow(&mut self, info: DatapathInfo, control: Datapath<T>) {
        self.install_fold(info.sock_id, &control);
        self.subflow.insert(info.sock_id, control);
        self.subflow_rtt.insert(info.sock_id, 0);
        self.subflow_pending.insert(info.sock_id, DEFAULT_PENDING_BYTES);
        self.subflow_cwnd.insert(info.sock_id, DEFAULT_PENDING_BYTES);
        self.subflow_util.insert(info.sock_id, 0);
        self.subflow_inflight.insert(info.sock_id, 0);
        self.num_flows += 1;
        self.send_pattern();
    }
}

impl<T: Ipc> CongAlg<T> for AggregationExample<T> {
    type Config = AggregationExampleConfig;

    fn name() -> String {
        String::from("aggregation")
    }

    fn create(control: Datapath<T>, cfg: Config<T, AggregationExample<T>>, info: DatapathInfo) -> Self {
        let mut s = Self {
            logger: cfg.logger,
            cwnd: info.init_cwnd,
            init_cwnd: info.init_cwnd,
            curr_cwnd_reduction: 0,
            ss_thresh: DEFAULT_SS_THRESH,
            sc: None,
            subflow: HashMap::new(),
            subflow_pending: HashMap::new(),
            subflow_rtt: HashMap::new(),
            subflow_cwnd: HashMap::new(),
            subflow_util: HashMap::new(),
            subflow_inflight: HashMap::new(),
            num_flows: 0,
        };

        s.sc = s.install_fold(info.sock_id, &control);
        s.subflow.insert(info.sock_id, control);

        s.logger.as_ref().map(|log| {
            debug!(log, "starting new aggregate"; "flow_sock_id" => info.sock_id);
        });

        s.send_pattern();
        s
    }

    fn measurement(&mut self, sock_id: u32, m: Measurement) {
        let (acked, was_timeout, sacked, loss, rtt, inflight, pending) = self.get_fields(m);

        self.subflow_rtt.insert(sock_id, rtt);
        self.subflow_pending.insert(sock_id, pending);
        self.subflow_util.insert(sock_id, acked+sacked);
        self.subflow_inflight.insert(sock_id, inflight);

        if was_timeout {
            self.handle_timeout();
            return;
        }

        // increase the cwnd corresponding to new in-order cumulative ACKs
        self.additive_increase_with_slow_start(acked);

        if loss > 0 || sacked > 0 {
            self.cwnd_reduction(loss, sacked, acked);
        } else if acked < self.curr_cwnd_reduction {
            self.curr_cwnd_reduction -= acked / 1448u32;
        } else {
            self.curr_cwnd_reduction = 0;
        }

        if self.curr_cwnd_reduction > 0 {
            self.logger.as_ref().map(|log| {
                debug!(log, "in cwnd reduction"; "acked" => acked / 1448u32, "deficit" => self.curr_cwnd_reduction);
            });
            return;
        }

        self.send_pattern();

        // self.logger.as_ref().map(|log| {
        //     debug!(log, "got ack";
        //         "flow sock_id" => sock_id,
        //         "acked(pkts)" => acked / 1448u32, 
        //         "curr_cwnd (pkts)" => self.cwnd / 1460, 
        //         "inflight (pkts)" => inflight, 
        //         "loss" => loss, 
        //         "rtt" => rtt,
        //         //"prior_cwnd" => prior_cwnd,
        //         "acked+sacked" => acked+sacked,
        //     );
        // });
    }
}

impl<T: Ipc> AggregationExample<T> {
    /* Function that determines congestion window per flow */
    /* For now, simplistically just divide the overall cwnd by the number of */
    /* flows. Can only be called when the connection vector is non-empty */
    fn get_window_rr(&mut self) -> u32 {
        // Currently, window is independent of the socket (split evenly)
        self.cwnd / (self.subflow.len() as u32)
    }

    fn get_average_rtt(&self) -> u32 {
        let mut average = 0;
        if self.num_flows == 0 {
            return 0;
        }
        /* The code below is guaranteed to run when there is at least one flow */
        /* in the aggregate. */
        for (_sock_id, rtt) in &self.subflow_rtt {
            average += rtt;
        }
        return average / self.num_flows;
    }

    /* Choose the pattern that needs to be sent to control flow scheduler here. */
    fn send_pattern(&mut self) {
        // self.send_pattern_alloc_srpt();
        // self.send_pattern_alloc_rr();
        self.send_pattern_alloc_maxmin();
        // self.send_pattern_alloc_proportional();
    }

    /* Beginnings of credit-based window allocator */
    // fn send_pattern_alloc_credit_maxmin(&mut self) {
    //     let mut total_demand = self.subflow_util.
    //         iter().fold(0, |sum, x| { sum + (*x.1) });
        
    // }

    fn get_demand_vec(&self) -> Vec<(u32, u32)> {
        let mut demand_vec : Vec<(u32, u32)> = self.
            subflow_pending.clone().into_iter().collect();
        /* Other smart stuff in projecting demands can happen here. But for
         * now, just sort and return.  */
        demand_vec.sort_by(|a, b| { a.1.cmp(&b.1) });
        demand_vec
    }

    fn send_pattern_alloc_maxmin(&mut self) {
        let demand_vec : Vec<_> = self.get_demand_vec();
        // let mut demand_vec : Vec<_> = self.subflow_pending.iter().collect();
        // demand_vec.sort_by(|a, b| { a.1.cmp(b.1) });
        let mut available_cwnd = self.cwnd;
        let mut num_flows_to_allocate = self.num_flows;
        for (sock_id, demand) in demand_vec { // sorted traversal
            if demand < available_cwnd / num_flows_to_allocate {
                self.subflow_cwnd.insert(sock_id, demand);
                // self.logger.as_ref().map(|log| {
                //     info!(log, "maxmin_alloc";
                //           "sock" => sock_id,
                //           "demand" => demand,
                //           "allocated window" => demand);
                // });
                available_cwnd -= demand;
                num_flows_to_allocate -= 1;
            } else {
                self.subflow_cwnd.insert(sock_id, available_cwnd / num_flows_to_allocate);
                // self.logger.as_ref().map(|log| {
                //     info!(log, "maxmin_alloc";
                //           "sock" => sock_id,
                //           "demand" => demand,
                //           "allocated window" => available_cwnd / num_flows_to_allocate);
                // });
            }
        }
        self.send_pattern_alloc_messages();
    }
    

    fn send_pattern_alloc_proportional(&mut self) {
        /* we allocate the entire cwnd, but proportional to flow demands. */
        let mut total_demand = self.subflow_pending.
            iter().fold(0, |sum, x| { sum + (*x.1) });
        if total_demand > 0 {
            for (&sock_id, &demand) in &self.subflow_pending {
                self.logger.as_ref().map(|log| {
                    info!(log, "pf_alloc"; "sock" => sock_id, "demand" => demand);
                });
                let mut temp: u64 = (self.cwnd as u64) * (demand as u64);
                temp /= total_demand as u64;
                temp = std::cmp::min(temp, 2896 as u64);
                self.subflow_cwnd.insert(sock_id, temp as u32);
            }
            self.send_pattern_alloc_messages();
        } else {
           self.send_pattern_alloc_rr();
        }
    }

    /* Patterns are sent repeatedly to all connections that are part of an */
    /* aggregate. Loop over connections */
    fn send_pattern_alloc_rr(&mut self) {
        for (&sock_id, _) in &mut self.subflow_pending {
            self.subflow_cwnd.insert(sock_id, self.cwnd / self.num_flows);
        }
        self.send_pattern_alloc_messages();
    }

    /* Set congestion windows based on remaining demand, as estimated by
     * pending bytes. Simple version doesn't try to coordinate the reception of
     * different measurement messages: just simply resets all control patterns
     * on reception of a single measurement. */
    fn send_pattern_alloc_srpt(&mut self) {
        let mut demand_sum = 0;
        for (_sock_id, pending) in &self.subflow_pending {
            demand_sum += pending;
        }
        let mut demand_vec : Vec<_> = self.subflow_pending.iter().collect();
        /* Sort flows by demand if overall window demands exceeds available
         * aggregate window. */
        if demand_sum > self.cwnd {
            demand_vec.sort_by(|a, b| { a.1.cmp(b.1) });
        }
        /* Must allocate in order that demand_vec allows */
        let mut allocated_cwnd = 0;
        for (&sock_id, &flow_demand) in demand_vec {
            let mut flow_cwnd;
            /* Perform allocation in order of demands, keeping larger flows out
             * if necessary */
            if allocated_cwnd < self.cwnd {
                if allocated_cwnd + flow_demand < self.cwnd {
                    flow_cwnd = flow_demand;
                    allocated_cwnd += flow_demand;
                } else {
                    flow_cwnd = self.cwnd - allocated_cwnd;
                    allocated_cwnd = self.cwnd;
                }
            } else {
                flow_cwnd = 1; // keep a small number of packets in flight anyway
            }
            self.subflow_cwnd.insert(sock_id, flow_cwnd);
        }
        self.send_pattern_alloc_messages();
    }

    /* Patterns are sent repeatedly to all connections that are part of an */
    /* aggregate. Loop over connections */
    fn send_pattern_sched_rr(&mut self) {
        let mut count = 0;
        let num_flows = self.subflow.len() as u32;
        let low_cwnd = 2; // number of packets in "off" phase of RR
        if num_flows == 0 {
            return;
        }
        let rr_interval_ns = self.get_average_rtt() * 1000;
        let flow_cwnd = self.get_window_rr();
        for (&sock_id, ref control_channel) in &self.subflow {
            count = count + 1;
            let begin_off_time = rr_interval_ns * (count - 1);
            let end_off_time = rr_interval_ns * (num_flows - count);
            let xmit_time = rr_interval_ns;
            self.logger.as_ref().map(|log| {
                info!(log, "sending"; "begin_off" => begin_off_time, "xmit" => xmit_time, "end_off" => end_off_time);
            });
            match control_channel.send_pattern(
                sock_id,
                make_pattern!(
                    pattern::Event::SetCwndAbs(low_cwnd) =>
                    pattern::Event::WaitNs(begin_off_time) =>
                    pattern::Event::SetCwndAbs(flow_cwnd) =>
                    pattern::Event::WaitNs(xmit_time) =>
                    pattern::Event::SetCwndAbs(low_cwnd) =>
                    pattern::Event::WaitNs(end_off_time) =>
                    pattern::Event::Report
                ),
            ) {
                Ok(_) => (),
                Err(e) => {
                    self.logger.as_ref().map(|log| {
                        warn!(log, "send_pattern"; "err" => ?e);
                    });
                }
            }
        };
    }

    fn send_pattern_alloc_messages(&self) {
        for (&sock_id, &flow_cwnd) in &self.subflow_cwnd {
            self.subflow.
                get(&sock_id).
                and_then(|control_channel| {
                    match control_channel.send_pattern(
                        sock_id,
                        make_pattern!(
                            pattern::Event::SetCwndAbs(flow_cwnd) =>
                            pattern::Event::WaitRtts(1.0) =>
                            pattern::Event::Report
                        ),
                    ) {
                        Ok(_) => (),
                        Err(e) => {
                            self.logger.as_ref().map(|log| {
                                warn!(log, "send_pattern"; "err" => ?e);
                            });
                            ()
                        }
                    };
                    // self.logger.as_ref().map(|log| {
                    //     info!(log, "send_pattern"; "sock" => sock_id, "cwnd" => flow_cwnd);
                    // });
                    Some(())
                })
                .or_else(|| {
                    Some(())
                });
        }
    }

    /* Install fold once for each connection */
    fn install_fold(&self, sock_id: u32, control_channel: &Datapath<T>) -> Option<Scope> {
        match control_channel.install_measurement(
            sock_id,
            "
                (def (acked 0) (sacked 0) (loss 0) (timeout false) (rtt 0) (inflight 0) (pending 0))
                (bind Flow.inflight Pkt.packets_in_flight)
                (bind Flow.rtt Pkt.rtt_sample_us)
                (bind Flow.acked (+ Flow.acked Pkt.bytes_acked))
                (bind Flow.sacked (+ Flow.sacked Pkt.packets_misordered))
                (bind Flow.loss Pkt.lost_pkts_sample)
                (bind Flow.timeout Pkt.was_timeout)
                (bind Flow.pending Pkt.bytes_pending)
                (bind isUrgent Pkt.was_timeout)
                (bind isUrgent (!if isUrgent (> Flow.loss 0)))
             "
                .as_bytes(),
        ) {
            Ok(s) => Some(s),
            Err(_) => None,
        }
    }

    fn get_fields(&mut self, m: Measurement) -> (u32, bool, u32, u32, u32, u32, u32) {
        let sc = self.sc.as_ref().expect("scope should be initialized");
        let ack = m.get_field(&String::from("Flow.acked"), sc).expect(
            "expected acked field in returned measurement",
        ) as u32;

        let sack = m.get_field(&String::from("Flow.sacked"), sc).expect(
            "expected sacked field in returned measurement",
        ) as u32;

        let was_timeout = m.get_field(&String::from("Flow.timeout"), sc).expect(
            "expected timeout field in returned measurement",
        ) as u32;

        let inflight = m.get_field(&String::from("Flow.inflight"), sc).expect(
            "expected inflight field in returned measurement",
        ) as u32;

        let loss = m.get_field(&String::from("Flow.loss"), sc).expect(
            "expected loss field in returned measurement",
        ) as u32;

        let rtt = m.get_field(&String::from("Flow.rtt"), sc).expect(
            "expected rtt field in returned measurement",
        ) as u32;

        let pending = m.get_field(&String::from("Flow.pending"), sc).expect(
            "expected pending field in returned measurement",
        ) as u32;

        (ack, was_timeout == 1, sack, loss, rtt, inflight, pending)
    }

    fn additive_increase_with_slow_start(&mut self, acked: u32) {
        let mut new_bytes_acked = acked;
        if self.cwnd < self.ss_thresh {
            // increase cwnd by 1 per packet, until ssthresh
            if self.cwnd + new_bytes_acked > self.ss_thresh {
                new_bytes_acked -= self.ss_thresh - self.cwnd;
                self.cwnd = self.ss_thresh;
            } else {
                self.cwnd += new_bytes_acked;
                new_bytes_acked = 0;
            }
        }

        // increase cwnd by 1 / cwnd per packet
        self.cwnd += 1448u32 * self.num_flows * (new_bytes_acked / self.cwnd);
    }

    fn handle_timeout(&mut self) {
        self.ss_thresh /= 2;
        if self.ss_thresh < self.init_cwnd {
            self.ss_thresh = self.init_cwnd;
        }

        let num_flows = self.subflow.len() as u32;
        // Congestion window update to reflect timeout for one flow
        self.cwnd = ((self.cwnd * (num_flows-1)) / num_flows) + self.init_cwnd;
        self.curr_cwnd_reduction = 0;

        self.logger.as_ref().map(|log| {
            warn!(log, "timeout"; 
                "curr_cwnd (pkts)" => self.cwnd / 1448, 
                "ssthresh" => self.ss_thresh,
            );
        });

        self.send_pattern();
        return;
    }

    /// Handle sacked or lost packets
    /// Only call with loss > 0 || sacked > 0
    fn cwnd_reduction(&mut self, loss: u32, sacked: u32, acked: u32) {
        // if loss indicator is nonzero
        // AND the losses in the lossy cwnd have not yet been accounted for
        // OR there is a partial ACK AND cwnd was probing ss_thresh
        if loss > 0 && self.curr_cwnd_reduction == 0 || (acked > 0 && self.cwnd == self.ss_thresh) {
            let num_flows = self.subflow.len() as u32;
            self.cwnd -= self.cwnd / (2 * num_flows);
            // self.cwnd /= 2;
            if self.cwnd <= self.init_cwnd {
                self.cwnd = self.init_cwnd;
            }

            self.ss_thresh = self.cwnd;
            self.send_pattern();
        }

        self.curr_cwnd_reduction += sacked + loss;
        self.logger.as_ref().map(|log| {
            info!(log, "loss"; "curr_cwnd (pkts)" => self.cwnd / 1448, "loss" => loss, "sacked" => sacked, "curr_cwnd_deficit" => self.curr_cwnd_reduction);
        });
    }
}