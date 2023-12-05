use std::marker::PhantomData;
use std::time::Duration;

use bevy::prelude::{
    apply_deferred, App, IntoSystemConfigs, IntoSystemSetConfigs, Plugin, PreUpdate, SystemSet,
};

use crate::client::components::SyncComponent;
use crate::client::interpolation::despawn::{
    despawn_interpolated, removed_components, InterpolationMapping,
};
use crate::client::interpolation::interpolate::{interpolate, update_interpolate_status};
use crate::protocol::component::ComponentProtocol;
use crate::protocol::Protocol;
use crate::shared::sets::MainSet;

use super::interpolation_history::{add_component_history, apply_confirmed_update};
use super::{spawn_interpolated_entity, InterpolatedComponent};

// TODO: maybe this is not an enum and user can specify multiple values, and we use the max delay between all of them?
#[derive(Clone)]
pub struct InterpolationDelay {
    /// The minimum delay that we will apply for interpolation
    /// This should be big enough so that the interpolated entity always has a server snapshot
    /// to interpolate towards.
    /// Set to 0.0 if you want to only use the Ratio
    pub min_delay: Duration,
    /// The interpolation delay is a ratio of the update-rate from the server
    /// The higher the server update_rate (i.e. smaller send_interval), the smaller the interpolation delay
    /// Set to 0.0 if you want to only use the Delay
    pub send_interval_ratio: f32,
}

impl Default for InterpolationDelay {
    fn default() -> Self {
        Self {
            min_delay: Duration::from_millis(0),
            send_interval_ratio: 2.0,
        }
    }
}

impl InterpolationDelay {
    pub fn with_min_delay(mut self, min_delay: Duration) -> Self {
        self.min_delay = min_delay;
        self
    }

    pub fn with_send_interval_ratio(mut self, send_interval_ratio: f32) -> Self {
        self.send_interval_ratio = send_interval_ratio;
        self
    }

    /// How much behind the latest server update we want the interpolation time to be
    pub(crate) fn to_duration(&self, server_send_interval: Duration) -> Duration {
        let ratio_value = server_send_interval.mul_f32(self.send_interval_ratio);
        std::cmp::max(ratio_value, self.min_delay)
    }
}

/// How much behind the client time the interpolated entities are
/// This will be converted to a tick
/// This should be
#[derive(Clone)]
pub struct InterpolationConfig {
    pub(crate) delay: InterpolationDelay,
    // How long are we keeping the history of the confirmed entities so we can interpolate between them?
    // pub(crate) interpolation_buffer_size: Duration,
}

#[allow(clippy::derivable_impls)]
impl Default for InterpolationConfig {
    fn default() -> Self {
        Self {
            delay: InterpolationDelay::default(),
            // TODO: change
            // interpolation_buffer_size: Duration::from_millis(100),
        }
    }
}

impl InterpolationConfig {
    pub fn with_delay(mut self, delay: InterpolationDelay) -> Self {
        self.delay = delay;
        self
    }
}

pub struct InterpolationPlugin<P: Protocol> {
    config: InterpolationConfig,

    // minimum_snapshots
    _marker: PhantomData<P>,
}

impl<P: Protocol> InterpolationPlugin<P> {
    pub(crate) fn new(config: InterpolationConfig) -> Self {
        Self {
            config,
            _marker: PhantomData,
        }
    }
}

impl<P: Protocol> Default for InterpolationPlugin<P> {
    fn default() -> Self {
        Self {
            config: InterpolationConfig::default(),
            _marker: PhantomData,
        }
    }
}

#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone, Copy)]
pub enum InterpolationSet {
    // PreUpdate Sets
    // // Contains the other pre-update prediction sets
    // PreUpdateInterpolation,
    /// Spawn interpolation entities,
    SpawnInterpolation,
    SpawnInterpolationFlush,
    /// Add component history for all interpolated entities' interpolated components
    SpawnHistory,
    SpawnHistoryFlush,
    /// Set to handle interpolated/confirmed entities/components getting despawned
    Despawn,
    DespawnFlush,
    /// Update component history, interpolation status, and interpolate between last 2 server states
    Interpolate,
}

// We want to run prediction:
// - after we received network events (PreUpdate)
// - before we run physics FixedUpdate (to not have to redo-them)

// - a PROBLEM is that ideally we would like to rollback the physics simulation
//   up to the client tick before we just updated the time. Maybe that's not a problem.. but we do need to keep track of the ticks correctly
//  the tick we rollback to would not be the current client tick ?

pub fn add_interpolation_systems<C: SyncComponent, P: Protocol>(app: &mut App) {
    // TODO: maybe create an overarching prediction set that contains all others?
    app.add_systems(
        PreUpdate,
        (
            (add_component_history::<C, P>).in_set(InterpolationSet::SpawnHistory),
            (removed_components::<C>).in_set(InterpolationSet::Despawn),
            (
                apply_confirmed_update::<C, P>,
                update_interpolate_status::<C, P>,
            )
                .chain()
                .in_set(InterpolationSet::Interpolate),
        ),
    );
}

// We add the interpolate system in different function because we don't want the non
// ComponentSyncMode::Full components to need the InterpolatedComponent bounds (in particular Add/Mul)
pub fn add_lerp_systems<C: InterpolatedComponent, P: Protocol>(app: &mut App) {
    app.add_systems(
        PreUpdate,
        (interpolate::<C>
            .after(update_interpolate_status::<C, P>)
            .in_set(InterpolationSet::Interpolate),),
    );
}

impl<P: Protocol> Plugin for InterpolationPlugin<P> {
    fn build(&self, app: &mut App) {
        P::Components::add_interpolation_systems(app);

        // RESOURCES
        app.init_resource::<InterpolationMapping>();
        // SETS
        app.configure_sets(
            PreUpdate,
            (
                MainSet::Receive,
                InterpolationSet::SpawnInterpolation,
                InterpolationSet::SpawnInterpolationFlush,
                InterpolationSet::SpawnHistory,
                InterpolationSet::SpawnHistoryFlush,
                InterpolationSet::Despawn,
                InterpolationSet::DespawnFlush,
                // TODO: maybe run in a schedule in-between FixedUpdate and Update?
                //  or maybe run during PostUpdate?
                InterpolationSet::Interpolate,
            )
                .chain(),
        );
        // SYSTEMS
        app.add_systems(
            PreUpdate,
            (
                // TODO: we want to run these flushes only if something actually happened in the previous set!
                //  because running the flush-system is expensive (needs exclusive world access)
                //  check how I can do this in bevy
                apply_deferred.in_set(InterpolationSet::SpawnInterpolationFlush),
                apply_deferred.in_set(InterpolationSet::SpawnHistoryFlush),
                apply_deferred.in_set(InterpolationSet::DespawnFlush),
            ),
        );
        app.add_systems(
            PreUpdate,
            (
                spawn_interpolated_entity.in_set(InterpolationSet::SpawnInterpolation),
                despawn_interpolated.in_set(InterpolationSet::Despawn),
            ),
        );
    }
}