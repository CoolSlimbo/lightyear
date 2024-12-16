//! Module to handle client inputs
//!
//! Client inputs are generated by the user and sent to the server.
//! They have to be handled separately from other messages, for several reasons:
//! - the history of inputs might need to be saved on the client to perform rollback and client-prediction
//! - we not only send the input for tick T, but we also include the inputs for the last N ticks before T. This redundancy helps ensure
//!   that the server isn't missing any client inputs even if a packet gets lost
//! - we must provide [`SystemSet`]s so that the user can order their systems before and after the input handling
//!
//! ### Adding a new input type
//!
//! An input type is an enum that implements the [`UserAction`] trait.
//! This trait is a marker trait that is used to tell Lightyear that this type can be used as an input.
//! In particular inputs must be `Serialize`, `Deserialize`, `Clone` and `PartialEq`.
//!
//! You can then add the input type by adding the [`InputPlugin<InputType>`](crate::prelude::InputPlugin) to your app.
//!
//! ```rust
//! use bevy::prelude::*;
//! use lightyear::prelude::*;
//!
//! #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
//! pub enum MyInput {
//!     Move { x: f32, y: f32 },
//!     Jump,
//!     // we need a variant for "no input", to differentiate between "no input" and "missing input packet"
//!     None,
//! }
//!
//! let mut app = App::new();
//! app.add_plugins(InputPlugin::<MyInput>::default());
//! ```
//!
//! ### Sending inputs
//!
//! There are several steps to use the `InputPlugin`:
//! - (optional) read the inputs from an external signal (mouse click or keyboard press, for instance)
//! - to buffer inputs for each tick. This is done by calling [`add_input`](InputManager::add_input) in a system.
//!   That system must run in the [`InputSystemSet::BufferInputs`] system set, in the `FixedPreUpdate` stage.
//! - handle inputs in your game logic in systems that run in the `FixedUpdate` schedule. These systems
//!   will read the inputs using the [`InputEvent`] event.
//!
//! NOTE: I would advise to activate the `leafwing` feature to handle inputs via the `input_leafwing` module, instead.
//! That module is more up-to-date and has more features.
//! This module is kept for simplicity but might get removed in the future.
use bevy::prelude::*;
use bevy::reflect::Reflect;
use bevy::utils::Duration;
use tracing::{debug, error, trace};

use crate::client::config::ClientConfig;
use crate::client::connection::ConnectionManager;
use crate::client::events::InputEvent;
use crate::client::prediction::plugin::is_in_rollback;
use crate::client::prediction::rollback::Rollback;
use crate::client::run_conditions::is_synced;
use crate::client::sync::SyncSet;
use crate::connection::client::NetClient;
use crate::connection::client::NetClientDispatch;
use crate::inputs::native::input_buffer::InputBuffer;
use crate::inputs::native::UserAction;
use crate::prelude::{is_host_server, ChannelKind, ChannelRegistry, Tick, TickManager};
use crate::shared::sets::{ClientMarker, InternalMainSet};
use crate::shared::tick_manager::TickEvent;
use crate::{channel::builder::InputChannel, prelude::client::ClientConnection};

#[derive(Debug, Clone, Copy, Reflect)]
pub struct InputConfig {
    /// How many consecutive packets losses do we want to handle?
    /// This is used to compute the redundancy of the input messages.
    /// For instance, a value of 3 means that each input packet will contain the inputs for all the ticks
    ///  for the 3 last packets.
    pub packet_redundancy: u16,
    /// How often do we send input messages to the server?
    /// Duration::default() means that we will send input messages every frame.
    pub send_interval: Duration,
}

/// Resource that handles buffering and sending inputs to the server
///
/// Note: it is advised to enable the feature `leafwing` and  switch to the `LeafwingInputPlugin`,
/// which is more up-to-date and has more features.
#[derive(Debug, Resource)]
pub struct InputManager<A> {
    pub(crate) input_buffer: InputBuffer<A>,
}

impl<A> Default for InputManager<A> {
    fn default() -> Self {
        Self {
            input_buffer: InputBuffer::default(),
        }
    }
}

impl<A: UserAction> InputManager<A> {
    /// Get a cloned version of the input (we might not want to pop from the buffer because we want
    /// to keep it for rollback)
    pub(crate) fn get_input(&self, tick: Tick) -> Option<A> {
        self.input_buffer.get(tick).cloned()
    }

    /// Buffer a user action for the given tick
    pub fn add_input(&mut self, input: A, tick: Tick) {
        self.input_buffer.set(tick, Some(input));
    }
}

impl Default for InputConfig {
    fn default() -> Self {
        InputConfig {
            packet_redundancy: 10,
            send_interval: Duration::default(),
        }
    }
}

pub struct InputPlugin<A: UserAction> {
    config: InputConfig,
    _marker: std::marker::PhantomData<A>,
}

impl<A: UserAction> InputPlugin<A> {
    fn new(config: InputConfig) -> Self {
        Self {
            config,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<A: UserAction> Default for InputPlugin<A> {
    fn default() -> Self {
        Self::new(InputConfig::default())
    }
}

/// Input of the user for the current tick
pub struct CurrentInput<A: UserAction> {
    // TODO: should we allow a Vec of inputs? for example if a user presses multiple buttons?
    //  or would that be encoded as a combination?
    input: A,
}

impl<A: UserAction> Plugin for InputPlugin<A> {
    fn build(&self, app: &mut App) {
        // REGISTRATION
        app.register_type::<InputConfig>();
        // RESOURCES
        app.init_resource::<InputManager<A>>();
        // EVENT
        app.add_event::<InputEvent<A>>();
        // SETS
        app.configure_sets(
            FixedPreUpdate,
            (
                // no need to keep buffering inputs during rollback
                InputSystemSet::BufferInputs.run_if(not(is_in_rollback)),
                InputSystemSet::WriteInputEvent,
            )
                .chain(),
        );
        app.configure_sets(FixedPostUpdate, InputSystemSet::ClearInputEvent);
        app.configure_sets(
            PostUpdate,
            (
                // create input messages after SyncSet to make sure that the TickEvents are handled
                SyncSet,
                // we send inputs only every send_interval
                InputSystemSet::SendInputMessage.run_if(
                    // no need to send input messages via io if we are in host-server mode
                    is_synced.and_then(not(is_host_server)),
                ),
                InternalMainSet::<ClientMarker>::Send,
            )
                .chain(),
        );
        // SYSTEMS

        // Host server mode only!
        app.add_systems(
            FixedPreUpdate,
            send_input_directly_to_client_events::<A>
                .in_set(InputSystemSet::WriteInputEvent)
                .run_if(is_host_server),
        );
        app.add_systems(
            FixedPreUpdate,
            write_input_event::<A>
                .in_set(InputSystemSet::WriteInputEvent)
                .run_if(not(is_host_server)),
        );
        app.add_systems(
            FixedPostUpdate,
            clear_input_events::<A>.in_set(InputSystemSet::ClearInputEvent),
        );
        app.add_observer(receive_tick_events::<A>);
        app.add_systems(
            PostUpdate,
            (prepare_input_message::<A>.in_set(InputSystemSet::SendInputMessage),),
        );

        // in case the framerate is faster than fixed-update interval, we also write/clear the events at frame limits
        // TODO: should we also write the events at PreUpdate?
        // app.add_systems(PostUpdate, clear_input_events::);
    }
}

#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone, Copy)]
pub enum InputSystemSet {
    // FIXED UPDATE
    /// System Set to write the input events to the input buffer.
    /// The User should add their system here!!
    BufferInputs,
    /// FixedUpdate system to get any inputs from the client. This should be run before the game/physics logic
    /// We access inputs via Events because of rollbacks: during rollbacks, we will re-emit past inputs as [`InputEvent`]s
    WriteInputEvent,
    /// System Set to clear the input events (otherwise bevy clears events every frame, not every tick)
    ClearInputEvent,

    // POST UPDATE
    /// System Set to prepare the input message (in Send SystemSet)
    SendInputMessage,
}

/// System that clears the input events.
/// It is necessary because events are cleared every frame, but we want to clear every tick instead
fn clear_input_events<A: UserAction>(mut input_events: EventReader<InputEvent<A>>) {
    input_events.clear();
}

// Create a system that reads from the input buffer and returns the inputs of all clients for the current tick.
// The only tricky part is that events are cleared every frame, but we want to clear every tick instead
// Do it in this system because we want an input for every tick
fn write_input_event<A: UserAction>(
    tick_manager: Res<TickManager>,
    input_manager: Res<InputManager<A>>,
    mut client_input_events: EventWriter<InputEvent<A>>,
    rollback: Option<Res<Rollback>>,
) {
    let tick = rollback.map_or(tick_manager.tick(), |r| {
        tick_manager.tick_or_rollback_tick(r.as_ref())
    });
    let input = input_manager.get_input(tick);
    client_input_events.send(InputEvent::new(input_manager.get_input(tick), ()));
}

/// Receive an [`TickEvent`] signifying that the local tick has been updated,
/// and update the input buffer accordingly
fn receive_tick_events<A: UserAction>(
    trigger: Trigger<TickEvent>,
    mut input_manager: ResMut<InputManager<A>>,
) {
    match trigger.event() {
        TickEvent::TickSnap { old_tick, new_tick } => {
            // if the tick got updated, update our inputs to match our new ticks
            if let Some(start_tick) = input_manager.input_buffer.start_tick {
                trace!(
                    "Receive tick snap event {:?}. Updating input buffer start_tick!",
                    trigger.event()
                );
                input_manager.input_buffer.start_tick = Some(start_tick + (*new_tick - *old_tick));
            };
        }
    }
}

/// Take the input buffer, and prepare the input message to send to the server
fn prepare_input_message<A: UserAction>(
    connection: Option<ResMut<ConnectionManager>>,
    channel_registry: Res<ChannelRegistry>,
    mut input_manager: ResMut<InputManager<A>>,
    config: Res<ClientConfig>,
    tick_manager: Res<TickManager>,
) {
    let Some(mut connection) = connection else {
        return;
    };

    let current_tick = tick_manager.tick();
    // TODO: the number of messages should be in SharedConfig
    trace!(tick = ?current_tick, "prepare_input_message");
    // TODO: instead of 15, send ticks up to the latest yet ACK-ed input tick
    //  this means we would also want to track packet->message acks for unreliable channels as well, so we can notify
    //  this system what the latest acked input tick is?

    // we send redundant inputs, so that if a packet is lost, we can still recover
    let input_send_interval = channel_registry
        .get_builder_from_kind(&ChannelKind::of::<InputChannel>())
        .unwrap()
        .settings
        .send_frequency;
    let num_tick: u16 =
        ((input_send_interval.as_nanos() / config.shared.tick.tick_duration.as_nanos()) + 1)
            .try_into()
            .unwrap();
    let redundancy = config.input.packet_redundancy;
    // let redundancy = 3;
    let message_len = redundancy * num_tick;
    // TODO: we can either:
    //  - buffer an input message at every tick, and not require that much redundancy
    //  - buffer an input every frame; and require some redundancy (number of tick per frame)
    //  - or buffer an input only when we are sending, and require more redundancy
    // let message_len = 20 as u16;
    let mut message = input_manager
        .input_buffer
        .create_message(tick_manager.tick(), message_len);
    // all inputs are absent
    if !message.is_empty() {
        // TODO: should we provide variants of each user-facing function, so that it pushes the error
        //  to the ConnectionEvents?
        debug!(
            ?current_tick,
            "sending input message: {:?}", message.end_tick
        );
        connection
            .send_message::<InputChannel, _>(&mut message)
            .unwrap_or_else(|err| {
                error!("Error while sending input message: {:?}", err);
            })
    }
    // NOTE: actually we keep the input values! because they might be needed when we rollback for client prediction
    // TODO: figure out when we can delete old inputs. Basically when the oldest prediction group tick has passed?
    //  maybe at interpolation_tick(), since it's before any latest server update we receive?

    // delete old input values
    let interpolation_tick = connection.sync_manager.interpolation_tick(&tick_manager);
    input_manager.input_buffer.pop(interpolation_tick);
    // .pop(current_tick - (message_len + 1));
}

/// In host server mode, we don't buffer inputs (because there is no rollback) and we don't send
/// inputs through the network, we just send directly to the server's InputEvents
fn send_input_directly_to_client_events<A: UserAction>(
    tick_manager: Res<TickManager>,
    client: Res<ClientConnection>,
    mut input_manager: ResMut<InputManager<A>>,
    mut server_input_events: EventWriter<crate::server::events::InputEvent<A>>,
) {
    if let NetClientDispatch::Local(client) = &client.client {
        let tick = tick_manager.tick();
        let input = input_manager.input_buffer.pop(tick);
        let event = crate::server::events::InputEvent::new(input, client.id());
        server_input_events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use crate::client::input::native::InputSystemSet;
    use crate::prelude::client::InputManager;
    use crate::prelude::{server, TickManager};
    use crate::tests::host_server_stepper::HostServerStepper;
    use crate::tests::protocol::MyInput;
    use bevy::prelude::*;

    fn press_input(
        mut input_manager: ResMut<InputManager<MyInput>>,
        tick_manager: Res<TickManager>,
    ) {
        input_manager.add_input(MyInput(2), tick_manager.tick());
    }

    #[derive(Resource)]
    pub struct Counter(pub u32);

    fn receive_input(
        mut counter: ResMut<Counter>,
        mut input: EventReader<server::InputEvent<MyInput>>,
    ) {
        for input in input.read() {
            assert_eq!(input.input().unwrap(), MyInput(2));
            counter.0 += 1;
        }
    }

    /// Check that in host-server mode the native client inputs from the buffer
    /// are forwarded directly to the server's InputEvents
    #[test]
    fn test_host_server_input() {
        let mut stepper = HostServerStepper::default_no_init();
        stepper.server_app.world_mut().insert_resource(Counter(0));
        stepper.server_app.add_systems(
            FixedPreUpdate,
            press_input.in_set(InputSystemSet::BufferInputs),
        );
        stepper.server_app.add_systems(FixedUpdate, receive_input);
        stepper.init();

        stepper.frame_step();
        assert!(stepper.server_app.world().resource::<Counter>().0 > 0);
    }
}
