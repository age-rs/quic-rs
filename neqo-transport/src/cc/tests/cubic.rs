// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "OK in tests."
)]

use std::{
    ops::Sub,
    time::{Duration, Instant},
};

use test_fixture::now;

use super::{IP_ADDR, MTU, RTT};
use crate::{
    cc::{
        classic_cc::ClassicCongestionControl,
        cubic::{
            convert_to_f64, Cubic, CUBIC_ALPHA, CUBIC_BETA_USIZE_DIVIDEND,
            CUBIC_BETA_USIZE_DIVISOR, CUBIC_C, CUBIC_FAST_CONVERGENCE,
        },
        CongestionControl as _,
    },
    packet,
    pmtud::Pmtud,
    recovery::{self, sent},
    rtt::RttEstimate,
};

const fn cwnd_after_loss(cwnd: usize) -> usize {
    cwnd * CUBIC_BETA_USIZE_DIVIDEND / CUBIC_BETA_USIZE_DIVISOR
}

const fn cwnd_after_loss_slow_start(cwnd: usize, mtu: usize) -> usize {
    (cwnd + mtu) * CUBIC_BETA_USIZE_DIVIDEND / CUBIC_BETA_USIZE_DIVISOR
}

fn fill_cwnd(cc: &mut ClassicCongestionControl<Cubic>, mut next_pn: u64, now: Instant) -> u64 {
    while cc.bytes_in_flight() < cc.cwnd() {
        let sent = sent::Packet::new(
            packet::Type::Short,
            next_pn,
            now,
            true,
            recovery::Tokens::new(),
            cc.max_datagram_size(),
        );
        cc.on_packet_sent(&sent, now);
        next_pn += 1;
    }
    next_pn
}

fn ack_packet(cc: &mut ClassicCongestionControl<Cubic>, pn: u64, now: Instant) {
    let acked = sent::Packet::new(
        packet::Type::Short,
        pn,
        now,
        true,
        recovery::Tokens::new(),
        cc.max_datagram_size(),
    );
    cc.on_packets_acked(&[acked], &RttEstimate::from_duration(RTT), now);
}

fn packet_lost(cc: &mut ClassicCongestionControl<Cubic>, pn: u64) {
    const PTO: Duration = Duration::from_millis(120);
    let p_lost = sent::Packet::new(
        packet::Type::Short,
        pn,
        now(),
        true,
        recovery::Tokens::new(),
        cc.max_datagram_size(),
    );
    cc.on_packets_lost(None, None, PTO, &[p_lost], now());
}

fn expected_tcp_acks(cwnd_rtt_start: usize, mtu: usize) -> u64 {
    (f64::from(i32::try_from(cwnd_rtt_start).unwrap())
        / f64::from(i32::try_from(mtu).unwrap())
        / CUBIC_ALPHA)
        .round() as u64
}

#[test]
fn tcp_phase() {
    let mut cubic = ClassicCongestionControl::new(Cubic::default(), Pmtud::new(IP_ADDR, MTU));

    // change to congestion avoidance state.
    cubic.set_ssthresh(1);

    let mut now = now();
    let start_time = now;
    // helper variables to remember the next packet number to be sent/acked.
    let mut next_pn_send = 0;
    let mut next_pn_ack = 0;

    next_pn_send = fill_cwnd(&mut cubic, next_pn_send, now);

    // This will start with TCP phase.
    // in this phase cwnd is increase by CUBIC_ALPHA every RTT. We can look at it as
    // increase of MAX_DATAGRAM_SIZE every 1 / CUBIC_ALPHA RTTs.
    // The phase will end when cwnd calculated with cubic equation is equal to TCP estimate:
    // CUBIC_C * (n * RTT / CUBIC_ALPHA)^3 * MAX_DATAGRAM_SIZE = n * MAX_DATAGRAM_SIZE
    // from this n = sqrt(CUBIC_ALPHA^3/ (CUBIC_C * RTT^3)).
    let num_tcp_increases = (CUBIC_ALPHA.powi(3) / (CUBIC_C * RTT.as_secs_f64().powi(3)))
        .sqrt()
        .floor() as u64;

    for _ in 0..num_tcp_increases {
        let cwnd_rtt_start = cubic.cwnd();
        // Expected acks during a period of RTT / CUBIC_ALPHA.
        let acks = expected_tcp_acks(cwnd_rtt_start, cubic.max_datagram_size());
        // The time between acks if they are ideally paced over a RTT.
        let time_increase =
            RTT / u32::try_from(cwnd_rtt_start / cubic.max_datagram_size()).unwrap();

        for _ in 0..acks {
            now += time_increase;
            ack_packet(&mut cubic, next_pn_ack, now);
            next_pn_ack += 1;
            next_pn_send = fill_cwnd(&mut cubic, next_pn_send, now);
        }

        assert_eq!(cubic.cwnd() - cwnd_rtt_start, cubic.max_datagram_size());
    }

    // The next increase will be according to the cubic equation.

    let cwnd_rtt_start = cubic.cwnd();
    // cwnd_rtt_start has change, therefore calculate new time_increase (the time
    // between acks if they are ideally paced over a RTT).
    let time_increase = RTT / u32::try_from(cwnd_rtt_start / cubic.max_datagram_size()).unwrap();
    let mut num_acks = 0; // count the number of acks. until cwnd is increased by cubic.max_datagram_size().

    while cwnd_rtt_start == cubic.cwnd() {
        num_acks += 1;
        now += time_increase;
        ack_packet(&mut cubic, next_pn_ack, now);
        next_pn_ack += 1;
        next_pn_send = fill_cwnd(&mut cubic, next_pn_send, now);
    }

    // Make sure that the increase is not according to TCP equation, i.e., that it took
    // less than RTT / CUBIC_ALPHA.
    let expected_ack_tcp_increase = expected_tcp_acks(cwnd_rtt_start, cubic.max_datagram_size());
    assert!(num_acks < expected_ack_tcp_increase);

    // This first increase after a TCP phase may be shorter than what it would take by a regular
    // cubic phase, because of the proper byte counting and the credit it already had before
    // entering this phase. Therefore We will perform another round and compare it to expected
    // increase using the cubic equation.

    let cwnd_rtt_start_after_tcp = cubic.cwnd();
    let elapsed_time = now - start_time;

    // calculate new time_increase.
    let time_increase =
        RTT / u32::try_from(cwnd_rtt_start_after_tcp / cubic.max_datagram_size()).unwrap();
    let mut num_acks2 = 0; // count the number of acks. until cwnd is increased by MAX_DATAGRAM_SIZE.

    while cwnd_rtt_start_after_tcp == cubic.cwnd() {
        num_acks2 += 1;
        now += time_increase;
        ack_packet(&mut cubic, next_pn_ack, now);
        next_pn_ack += 1;
        next_pn_send = fill_cwnd(&mut cubic, next_pn_send, now);
    }

    let expected_ack_tcp_increase2 =
        expected_tcp_acks(cwnd_rtt_start_after_tcp, cubic.max_datagram_size());
    assert!(num_acks2 < expected_ack_tcp_increase2);

    // The time needed to increase cwnd by MAX_DATAGRAM_SIZE using the cubic equation will be
    // calculated from: W_cubic(elapsed_time + t_to_increase) - W_cubic(elapsed_time) =
    // MAX_DATAGRAM_SIZE => CUBIC_C * (elapsed_time + t_to_increase)^3 * MAX_DATAGRAM_SIZE +
    // CWND_INITIAL - CUBIC_C * elapsed_time^3 * MAX_DATAGRAM_SIZE + CWND_INITIAL =
    // MAX_DATAGRAM_SIZE => t_to_increase = cbrt((1 + CUBIC_C * elapsed_time^3) / CUBIC_C) -
    // elapsed_time (t_to_increase is in seconds)
    // number of ack needed is t_to_increase / time_increase.
    let expected_ack_cubic_increase =
        (((CUBIC_C.mul_add((elapsed_time).as_secs_f64().powi(3), 1.0) / CUBIC_C).cbrt()
            - elapsed_time.as_secs_f64())
            / time_increase.as_secs_f64())
        .ceil() as u64;
    // num_acks is very close to the calculated value. The exact value is hard to calculate
    // because the proportional increase (i.e. curr_cwnd_f64 / (target - curr_cwnd_f64) *
    // MAX_DATAGRAM_SIZE_F64) and the byte counting.
    assert_eq!(num_acks2, expected_ack_cubic_increase + 2);
}

#[test]
fn cubic_phase() {
    let mut cubic = ClassicCongestionControl::new(Cubic::default(), Pmtud::new(IP_ADDR, MTU));
    let cwnd_initial_f64 = convert_to_f64(cubic.cwnd_initial());
    // Set last_max_cwnd to a higher number make sure that cc is the cubic phase (cwnd is calculated
    // by the cubic equation).
    cubic.set_last_max_cwnd(cwnd_initial_f64 * 10.0);
    // Set ssthresh to something small to make sure that cc is in the congection avoidance phase.
    cubic.set_ssthresh(1);
    let mut now = now();
    let mut next_pn_send = 0;
    let mut next_pn_ack = 0;

    next_pn_send = fill_cwnd(&mut cubic, next_pn_send, now);

    let k = (cwnd_initial_f64.mul_add(10.0, -cwnd_initial_f64)
        / CUBIC_C
        / convert_to_f64(cubic.max_datagram_size()))
    .cbrt();
    let epoch_start = now;

    // The number of RTT until W_max is reached.
    let num_rtts_w_max = (k / RTT.as_secs_f64()).round() as u64;
    for _ in 0..num_rtts_w_max {
        let cwnd_rtt_start = cubic.cwnd();
        // Expected acks
        let acks = cwnd_rtt_start / cubic.max_datagram_size();
        let time_increase = RTT / u32::try_from(acks).unwrap();
        for _ in 0..acks {
            now += time_increase;
            ack_packet(&mut cubic, next_pn_ack, now);
            next_pn_ack += 1;
            next_pn_send = fill_cwnd(&mut cubic, next_pn_send, now);
        }

        let expected = (CUBIC_C * ((now - epoch_start).as_secs_f64() - k).powi(3))
            .mul_add(
                convert_to_f64(cubic.max_datagram_size()),
                cwnd_initial_f64 * 10.0,
            )
            .round() as usize;

        assert_within(cubic.cwnd(), expected, cubic.max_datagram_size());
    }
    assert_eq!(cubic.cwnd(), cubic.cwnd_initial() * 10);
}

fn assert_within<T: Sub<Output = T> + PartialOrd + Copy>(value: T, expected: T, margin: T) {
    if value >= expected {
        assert!(value - expected < margin);
    } else {
        assert!(expected - value < margin);
    }
}

#[test]
fn congestion_event_slow_start() {
    let mut cubic = ClassicCongestionControl::new(Cubic::default(), Pmtud::new(IP_ADDR, MTU));

    _ = fill_cwnd(&mut cubic, 0, now());
    ack_packet(&mut cubic, 0, now());

    assert_within(cubic.last_max_cwnd(), 0.0, f64::EPSILON);

    // cwnd is increased by 1 in slow start phase, after an ack.
    assert_eq!(
        cubic.cwnd(),
        cubic.cwnd_initial() + cubic.max_datagram_size()
    );

    // Trigger a congestion_event in slow start phase
    packet_lost(&mut cubic, 1);

    // last_max_cwnd is equal to cwnd before decrease.
    let cwnd_initial_f64 = convert_to_f64(cubic.cwnd_initial());
    assert_within(
        cubic.last_max_cwnd(),
        cwnd_initial_f64 + convert_to_f64(cubic.max_datagram_size()),
        f64::EPSILON,
    );
    assert_eq!(
        cubic.cwnd(),
        cwnd_after_loss_slow_start(cubic.cwnd_initial(), cubic.max_datagram_size())
    );
}

#[test]
fn congestion_event_congestion_avoidance() {
    let mut cubic = ClassicCongestionControl::new(Cubic::default(), Pmtud::new(IP_ADDR, MTU));

    // Set ssthresh to something small to make sure that cc is in the congection avoidance phase.
    cubic.set_ssthresh(1);

    // Set last_max_cwnd to something smaller than cwnd so that the fast convergence is not
    // triggered.
    cubic.set_last_max_cwnd(3.0 * convert_to_f64(cubic.max_datagram_size()));

    _ = fill_cwnd(&mut cubic, 0, now());
    ack_packet(&mut cubic, 0, now());

    assert_eq!(cubic.cwnd(), cubic.cwnd_initial());

    // Trigger a congestion_event in slow start phase
    packet_lost(&mut cubic, 1);

    let cwnd_initial_f64 = convert_to_f64(cubic.cwnd_initial());
    assert_within(cubic.last_max_cwnd(), cwnd_initial_f64, f64::EPSILON);
    assert_eq!(cubic.cwnd(), cwnd_after_loss(cubic.cwnd_initial()));
}

#[test]
fn congestion_event_congestion_avoidance_2() {
    let mut cubic = ClassicCongestionControl::new(Cubic::default(), Pmtud::new(IP_ADDR, MTU));

    // Set ssthresh to something small to make sure that cc is in the congection avoidance phase.
    cubic.set_ssthresh(1);

    // Set last_max_cwnd to something higher than cwnd so that the fast convergence is triggered.
    let cwnd_initial_f64 = convert_to_f64(cubic.cwnd_initial());
    cubic.set_last_max_cwnd(cwnd_initial_f64 * 10.0);

    _ = fill_cwnd(&mut cubic, 0, now());
    ack_packet(&mut cubic, 0, now());

    assert_within(cubic.last_max_cwnd(), cwnd_initial_f64 * 10.0, f64::EPSILON);
    assert_eq!(cubic.cwnd(), cubic.cwnd_initial());

    // Trigger a congestion_event.
    packet_lost(&mut cubic, 1);

    assert_within(
        cubic.last_max_cwnd(),
        cwnd_initial_f64 * CUBIC_FAST_CONVERGENCE,
        f64::EPSILON,
    );
    assert_eq!(cubic.cwnd(), cwnd_after_loss(cubic.cwnd_initial()));
}

#[test]
fn congestion_event_congestion_avoidance_no_overflow() {
    const PTO: Duration = Duration::from_millis(120);
    let mut cubic = ClassicCongestionControl::new(Cubic::default(), Pmtud::new(IP_ADDR, MTU));

    // Set ssthresh to something small to make sure that cc is in the congection avoidance phase.
    cubic.set_ssthresh(1);

    // Set last_max_cwnd to something higher than cwnd so that the fast convergence is triggered.
    let cwnd_initial_f64 = convert_to_f64(cubic.cwnd_initial());
    cubic.set_last_max_cwnd(cwnd_initial_f64 * 10.0);

    _ = fill_cwnd(&mut cubic, 0, now());
    ack_packet(&mut cubic, 1, now());

    assert_within(cubic.last_max_cwnd(), cwnd_initial_f64 * 10.0, f64::EPSILON);
    assert_eq!(cubic.cwnd(), cubic.cwnd_initial());

    // Now ack packet that was send earlier.
    ack_packet(&mut cubic, 0, now().checked_sub(PTO).unwrap());
}
