//! BBR-style bandwidth/probe model for the bespoke transport paths (extra strategy).
//!
//! DESIGN §0 says: do not reinvent L4 — reuse QUIC/RDMA. The one place bespoke
//! at the wire is legitimate is kernel-bypass (RDMA), where you don't get a
//! congestion controller for free and a "goodput" model still has to decide
//! how hard to push the fabric. This module is a tiny BBR-flavoured model for
//! those paths: estimate bottleneck bandwidth and RTprop from delivery-rate
//! samples, and run in `PROBE_BW` / `PROBE_RTT` phases to hold the true
//! optimum-inflight (the BDP) instead of sawtooth-ing like CUBIC.
//!
//! It is a *model* the transport layer consults, not a packet-level loop; it
//! composes with RDMA verbs rather than replacing them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Startup,
    Drain,
    ProbeBw,
    ProbeRtt,
}

/// A BBR-style control state.
pub struct BbrModel {
    phase: Phase,
    /// max measured bandwidth (bytes/s).
    bw: f64,
    /// min measured RTT (s).
    rtprop: f64,
    /// current pacing rate (bytes/s) derived from bw * gain.
    pacing_rate: f64,
    /// BBR cwnd gain cycles for PROBE_BW.
    cycle_idx: usize,
    /// samples since last RTProp refresh.
    rtprop_stamp: u64,
    inflight: f64,
}

const GAINS: [f64; 8] = [1.0, 0.75, 1.0, 1.0, 1.0, 0.75, 1.0, 1.0];

impl Default for BbrModel {
    fn default() -> Self {
        Self::new()
    }
}

impl BbrModel {
    pub fn new() -> Self {
        Self {
            phase: Phase::Startup,
            bw: 0.0,
            rtprop: f64::INFINITY,
            pacing_rate: 0.0,
            cycle_idx: 0,
            rtprop_stamp: 0,
            inflight: 0.0,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn pacing_rate(&self) -> f64 {
        self.pacing_rate
    }
    pub fn bandwidth(&self) -> f64 {
        self.bw
    }
    pub fn rtprop(&self) -> f64 {
        self.rtprop
    }
    /// Optimum in-flight = bandwidth * RTprop (the BDP).
    pub fn bdp(&self) -> f64 {
        self.bw * self.rtprop
    }

    /// Feed a delivery-rate sample: `delivered` bytes over `interval` seconds,
    /// with observed `rtt` seconds, at monotonic `now`.
    pub fn on_sample(&mut self, delivered: u64, interval: f64, rtt: f64, now: u64) {
        if interval <= 0.0 {
            return;
        }
        let rate = delivered as f64 / interval;
        // update bandwidth estimate (max filter)
        if self.phase == Phase::Startup {
            // 2x growth until bw stops growing
            self.bw = self.bw.max(rate);
        } else {
            self.bw = self.bw.max(rate);
        }
        // update RTprop (min filter)
        if rtt > 0.0 && rtt < self.rtprop {
            self.rtprop = rtt;
            self.rtprop_stamp = now;
        }
        // phase transitions
        match self.phase {
            Phase::Startup => {
                // when bw has plateaued (sample not exceeding 1.25x prior),
                // drain excess inflight then probe.
                if self.bw > 0.0 && rate < self.bw * 0.8 {
                    self.phase = Phase::Drain;
                }
            }
            Phase::Drain => {
                if self.inflight <= self.bdp() {
                    self.phase = Phase::ProbeBw;
                }
            }
            Phase::ProbeBw => {
                self.cycle_idx = (self.cycle_idx + 1) % GAINS.len();
                if now - self.rtprop_stamp > 10_000 {
                    self.phase = Phase::ProbeRtt;
                }
            }
            Phase::ProbeRtt => {
                if now - self.rtprop_stamp > 2_000 {
                    self.phase = Phase::ProbeBw;
                }
            }
        }
        let gain = match self.phase {
            Phase::Startup => 2.0,
            Phase::Drain => 0.5,
            Phase::ProbeBw => GAINS[self.cycle_idx],
            Phase::ProbeRtt => 0.5,
        };
        self.pacing_rate = self.bw * gain;
        self.inflight = self.bdp() * gain;
    }
}
