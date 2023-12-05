/*! Handles syncing the time between the client and the server
*/
use std::pin::pin;
use std::time::Duration;

use bevy::prelude::Res;
use bevy::time::Stopwatch;
use tracing::{debug, info, trace};

use crate::client::interpolation::plugin::InterpolationDelay;
use crate::client::resource::Client;
use crate::packet::packet::PacketId;
use crate::protocol::Protocol;
use crate::shared::ping::manager::PingManager;
use crate::shared::ping::message::{Ping, Pong};
use crate::shared::ping::store::{PingId, PingStore};
use crate::shared::tick_manager::Tick;
use crate::shared::tick_manager::TickManager;
use crate::shared::time_manager::{TimeManager, WrappedTime};
use crate::utils::ready_buffer::ReadyBuffer;

/// Run condition to run systems only if the client is synced
pub fn client_is_synced<P: Protocol>(client: Res<Client<P>>) -> bool {
    client.is_synced()
}

#[derive(Clone, Debug)]
pub struct SyncConfig {
    /// How much multiple of jitter do we apply as margin when computing the time
    /// a packet will get received by the server
    /// (worst case will be RTT / 2 + jitter * multiple_margin)
    /// % of packets that will be received within k * jitter
    /// 1: 65%, 2: 95%, 3: 99.7%
    pub jitter_multiple_margin: u8,
    /// How many ticks to we apply as margin when computing the time
    ///  a packet will get received by the server
    pub tick_margin: u8,
    /// Number of pings to exchange with the server before finalizing the handshake
    pub handshake_pings: u8,
    /// Duration of the rolling buffer of stats to compute RTT/jitter
    pub stats_buffer_duration: Duration,
    /// Error margin for upstream throttle (in multiple of ticks)
    pub error_margin: f32,
    // TODO: instead of constant speedup_factor, the speedup should be linear w.r.t the offset
    /// By how much should we speed up the simulation to make ticks stay in sync with server?
    pub speedup_factor: f32,

    // Integration
    current_server_time_smoothing: f32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            jitter_multiple_margin: 3,
            tick_margin: 1,
            handshake_pings: 7,
            stats_buffer_duration: Duration::from_secs(2),
            error_margin: 1.0,
            speedup_factor: 1.03,
            current_server_time_smoothing: 0.1,
        }
    }
}

impl SyncConfig {
    pub fn speedup_factor(mut self, speedup_factor: f32) -> Self {
        self.speedup_factor = speedup_factor;
        self
    }
}

#[derive(Default)]
pub struct SentPacketStore {
    buffer: ReadyBuffer<WrappedTime, PacketId>,
}

impl SentPacketStore {
    pub fn new() -> Self {
        Self {
            buffer: ReadyBuffer::new(),
        }
    }
}

/// In charge of syncing the client's tick/time with the server's tick/time
/// right after the connection is established
pub struct SyncManager {
    config: SyncConfig,
    /// whether the handshake is finalized
    pub(crate) synced: bool,

    // time
    current_server_time: WrappedTime,
    pub(crate) interpolation_time: WrappedTime,
    interpolation_speed_ratio: f32,

    // ticks
    // TODO: see if this is correct; should we instead attach the tick on every update message?
    /// Tick of the server that we last received in any packet from the server.
    /// This is not updated every tick, but only when we receive a packet from the server.
    /// (usually every frame)
    pub(crate) latest_received_server_tick: Tick,
    pub(crate) estimated_interpolation_tick: Tick,
    pub(crate) duration_since_latest_received_server_tick: Duration,
    pub(crate) new_latest_received_server_tick: bool,
}

// TODO: split into PredictionTime Manager, InterpolationTime Manager
impl SyncManager {
    pub fn new(config: SyncConfig) -> Self {
        Self {
            config: config.clone(),
            synced: false,
            // time
            current_server_time: WrappedTime::default(),
            interpolation_time: WrappedTime::default(),
            interpolation_speed_ratio: 1.0,
            // server tick
            latest_received_server_tick: Tick(0),
            estimated_interpolation_tick: Tick(0),
            duration_since_latest_received_server_tick: Duration::default(),
            new_latest_received_server_tick: false,
        }
    }

    /// We want to run this update at PostUpdate, after both ticks/time have been updated
    /// (because we need to compare the client tick with the server tick when the server sends packets,
    /// i.e. after both ticks/time have been updated)
    pub(crate) fn update(
        &mut self,
        time_manager: &mut TimeManager,
        tick_manager: &mut TickManager,
        ping_manager: &PingManager,
        interpolation_delay: &InterpolationDelay,
        server_send_interval: Duration,
    ) {
        self.duration_since_latest_received_server_tick += time_manager.delta();
        self.current_server_time += time_manager.delta();
        self.interpolation_time += time_manager.delta().mul_f32(self.interpolation_speed_ratio);

        // check if we are ready to finalize the handshake
        if !self.synced && ping_manager.sync_stats.len() >= self.config.handshake_pings as usize {
            info!("Received enough pongs to finalize handshake");
            self.synced = true;
            self.finalize(time_manager, tick_manager, ping_manager);
            self.interpolation_time = self.interpolation_objective(
                interpolation_delay,
                server_send_interval,
                tick_manager,
            )
        }

        if self.synced {
            self.update_interpolation_time(interpolation_delay, server_send_interval, tick_manager);
        }
    }

    pub(crate) fn is_synced(&self) -> bool {
        self.synced
    }

    /// Compute the current client time; we will make sure that the client tick is ahead of the server tick
    /// Even if it is wrapped around.
    /// (i.e. if client tick is 1, and server tick is 65535, we act as if the client tick was 65537)
    /// This is because we have 2 distinct entities with wrapping: Ticks and WrappedTime
    pub(crate) fn current_prediction_time(
        &self,
        tick_manager: &TickManager,
        time_manager: &TimeManager,
    ) -> WrappedTime {
        // NOTE: careful! We know that client tick should always be ahead of server tick.
        //  let's assume that this is the case after we did tick syncing
        //  so if we are behind, that means that the client tick wrapped around.
        //  for the purposes of the sync computations, the client tick should be ahead
        let mut client_tick_raw = tick_manager.current_tick().0 as i32;
        // TODO: fix this
        // client can only be this behind server if it wrapped around...
        if (self.latest_received_server_tick.0 as i32 - client_tick_raw) > i16::MAX as i32 - 1000 {
            client_tick_raw += u16::MAX as i32;
        }
        WrappedTime::from_duration(
            tick_manager.config.tick_duration * client_tick_raw as u32 + time_manager.overstep(),
        )
    }

    /// current server time from server's point of view (using server tick)
    pub(crate) fn current_server_time(&self) -> WrappedTime {
        // TODO: instead of just using the latest_received_server_tick, there should be some sort
        //  of integration/smoothing
        self.current_server_time
    }

    /// Everytime we receive a new server update:
    /// Update the estimated current server time, computed from the time elapsed since the
    /// latest received server tick, and our estimate of the RTT
    pub(crate) fn update_current_server_time(&mut self, tick_duration: Duration, rtt: Duration) {
        let new_current_server_time_estimate = WrappedTime::from_duration(
            self.latest_received_server_tick.0 as u32 * tick_duration
                + self.duration_since_latest_received_server_tick
                + rtt / 2,
        );
        // instead of just using the latest_received_server_tick, there should be some sort
        // of integration/smoothing
        // (in case the latest server tick is wildly off-base)
        if self.current_server_time == WrappedTime::default() {
            self.current_server_time = new_current_server_time_estimate;
        } else {
            self.current_server_time = self.current_server_time
                * self.config.current_server_time_smoothing
                + new_current_server_time_estimate
                    * (1.0 - self.config.current_server_time_smoothing);
        }
    }

    /// time at which the server would receive a packet we send now
    fn predicted_server_receive_time(&self, rtt: Duration) -> WrappedTime {
        self.current_server_time() + rtt / 2
    }

    /// how far ahead of the server should I be?
    fn client_ahead_minimum(&self, tick_duration: Duration, jitter: Duration) -> Duration {
        self.config.jitter_multiple_margin as u32 * jitter
            + self.config.tick_margin as u32 * tick_duration
    }

    pub(crate) fn estimated_interpolated_tick(&self) -> Tick {
        self.estimated_interpolation_tick
    }

    pub(crate) fn interpolation_objective(
        &self,
        // TODO: make interpolation delay part of SyncConfig?
        interpolation_delay: &InterpolationDelay,
        // TODO: should we get this via an estimate?
        server_send_interval: Duration,
        tick_manager: &TickManager,
    ) -> WrappedTime {
        // We want the interpolation time to be just a little bit behind the latest server time
        // We add `duration_since_latest_received_server_tick` because we receive them intermittently
        // TODO: maybe integrate?
        let objective_time = WrappedTime::from_duration(
            self.latest_received_server_tick.0 as u32 * tick_manager.config.tick_duration
                + self.duration_since_latest_received_server_tick,
        );
        // how much we want interpolation time to be behind the latest received server tick?
        // TODO: use a specified config margin + add std of time_between_server_updates?
        let objective_delta =
            chrono::Duration::from_std(interpolation_delay.to_duration(server_send_interval))
                .unwrap();
        objective_time - objective_delta
    }

    pub(crate) fn interpolation_tick(&self, tick_manager: &TickManager) -> Tick {
        Tick(
            (self.interpolation_time.elapsed_us_wrapped
                / tick_manager.config.tick_duration.as_micros() as u32) as u16,
        )
    }

    // TODO: only run when there's a change? (new server tick received or new ping received)
    // TODO: change name to make it clear that we might modify speed
    pub(crate) fn update_interpolation_time(
        &mut self,
        // TODO: make interpolation delay part of SyncConfig?
        interpolation_delay: &InterpolationDelay,
        // TODO: should we get this via an estimate?
        server_update_rate: Duration,
        tick_manager: &TickManager,
    ) {
        // for interpolation time, we don't need to use ticks (because we only need interpolation at the end
        // of the frame, not during the FixedUpdate schedule)
        let objective_time =
            self.interpolation_objective(interpolation_delay, server_update_rate, tick_manager);
        let delta = objective_time - self.interpolation_time;

        let error_margin = chrono::Duration::milliseconds(10);
        if delta > error_margin {
            // interpolation time is too far behind, speed-up!
            self.interpolation_speed_ratio = 1.0 * self.config.speedup_factor;
        } else if delta < -error_margin {
            self.interpolation_speed_ratio = 1.0 / self.config.speedup_factor;
        } else {
            self.interpolation_speed_ratio = 1.0;
        }
    }

    /// Update the client time ("upstream-throttle"): speed-up or down depending on the
    /// The objective of update-client-time is to make sure the client packets for tick T arrive on server before server reaches tick T
    /// but not too far ahead
    pub(crate) fn update_prediction_time(
        &mut self,
        time_manager: &mut TimeManager,
        tick_manager: &TickManager,
        ping_manager: &PingManager,
    ) {
        let rtt = ping_manager.rtt();
        let jitter = ping_manager.jitter();
        // current client time
        let current_prediction_time = self.current_prediction_time(tick_manager, time_manager);
        // time at which the server would receive a packet we send now
        // (or time at which the server's packet would arrive on the client, computed using server tick)
        let predicted_server_receive_time = self.predicted_server_receive_time(rtt);

        // how far ahead of the server am I?
        let client_ahead_delta = current_prediction_time - predicted_server_receive_time;
        // how far ahead of the server should I be?
        let client_ahead_minimum =
            self.client_ahead_minimum(tick_manager.config.tick_duration, jitter);

        // we want client_ahead_delta > 3 * RTT_stddev + N / tick_rate to be safe
        let error = client_ahead_delta - chrono::Duration::from_std(client_ahead_minimum).unwrap();
        let error_margin_time = chrono::Duration::from_std(
            tick_manager
                .config
                .tick_duration
                .mul_f32(self.config.error_margin),
        )
        .unwrap();

        time_manager.sync_relative_speed = if error > error_margin_time {
            debug!(
                ?rtt,
                ?jitter,
                ?current_prediction_time,
                latest_received_server_tick = ?self.latest_received_server_tick,
                client_tick = ?tick_manager.current_tick(),
                client_ahead_delta_ms = ?client_ahead_delta.num_milliseconds(),
                ?client_ahead_minimum,
                error_ms = ?error.num_milliseconds(),
                error_margin_time_ms = ?error_margin_time.num_milliseconds(),
                "Too far ahead of server! Slow down!",
            );
            // we are too far ahead of the server, slow down
            1.0 / self.config.speedup_factor
        } else if error < -error_margin_time {
            debug!(
                ?rtt,
                ?jitter,
                ?current_prediction_time,
                latest_received_server_tick = ?self.latest_received_server_tick,
                client_tick = ?tick_manager.current_tick(),
                client_ahead_delta_ms = ?client_ahead_delta.num_milliseconds(),
                ?client_ahead_minimum,
                error_ms = ?error.num_milliseconds(),
                error_margin_time_ms = ?error_margin_time.num_milliseconds(),
                "Too far behind of server! Speed up!",
            );
            // we are too far behind the server, speed up
            1.0 * self.config.speedup_factor
        } else {
            // we are within margins
            trace!("good speed");
            1.0
        };
    }

    // Update internal time using offset so that times are synced.
    // This happens when a necessary # of handshake pongs have been recorded
    // Compute the final RTT/offset and set the client tick accordingly
    pub fn finalize(
        &mut self,
        time_manager: &mut TimeManager,
        tick_manager: &mut TickManager,
        ping_manager: &PingManager,
    ) {
        let tick_duration = tick_manager.config.tick_duration;
        let rtt = ping_manager.rtt();
        let jitter = ping_manager.jitter();
        // recompute the current server time (using the rtt we just computed)
        self.update_current_server_time(tick_duration, rtt);

        // Compute how many ticks the client must be compared to server
        let client_ideal_time = self.predicted_server_receive_time(rtt)
            + self.client_ahead_minimum(tick_duration, jitter);
        // we add 1 to get the div_ceil
        let client_ideal_tick = Tick(
            (client_ideal_time.elapsed_us_wrapped / tick_duration.as_micros() as u32) as u16 + 1,
        );

        let delta_tick = client_ideal_tick - tick_manager.current_tick();
        // Update client ticks
        let latency = rtt / 2;
        info!(
            buffer_len = ?ping_manager.sync_stats.len(),
            ?latency,
            ?jitter,
            ?delta_tick,
            ?client_ideal_tick,
            server_tick = ?self.latest_received_server_tick,
            client_current_tick = ?tick_manager.current_tick(),
            "Finished syncing!"
        );
        tick_manager.set_tick_to(client_ideal_tick)
    }
}