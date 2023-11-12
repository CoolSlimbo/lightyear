pub(crate) mod sets;
mod systems;

use std::ops::DerefMut;
use std::sync::Mutex;

use bevy::prelude::{
    not, resource_exists, App, Condition, Fixed, FixedUpdate, IntoSystemConfigs,
    Plugin as PluginType, PostUpdate, PreUpdate, Time,
};

use crate::plugin::systems::tick::increment_tick;
use crate::replication::prediction::is_in_rollback;
use crate::{
    ConnectEvent, DisconnectEvent, EntitySpawnEvent, MessageProtocol, Protocol, ReplicationData,
    SharedPlugin,
};

use super::config::ClientConfig;
use crate::client::prediction::{Rollback, RollbackState};
use crate::client::{Authentication, Client};
use sets::ClientSet;
use systems::{receive, send};

pub struct PluginConfig<P: Protocol> {
    client_config: ClientConfig,
    protocol: P,
    auth: Authentication,
}

// TODO: put all this in ClientConfig?
impl<P: Protocol> PluginConfig<P> {
    pub fn new(client_config: ClientConfig, protocol: P, auth: Authentication) -> Self {
        PluginConfig {
            client_config,
            protocol,
            auth,
        }
    }
}
pub struct Plugin<P: Protocol> {
    // we add Mutex<Option> so that we can get ownership of the inner from an immutable reference
    // in build()
    config: Mutex<Option<PluginConfig<P>>>,
}

impl<P: Protocol> Plugin<P> {
    pub fn new(config: PluginConfig<P>) -> Self {
        Self {
            config: Mutex::new(Some(config)),
        }
    }
}

impl<P: Protocol> PluginType for Plugin<P> {
    fn build(&self, app: &mut App) {
        let config = self.config.lock().unwrap().deref_mut().take().unwrap();
        let client = Client::new(config.client_config.clone(), config.auth, config.protocol);
        let fixed_timestep = config.client_config.shared.tick.tick_duration.clone();

        // TODO: it's annoying to have to keep that () around...
        //  revisit this.. maybe the into_iter_messages returns directly an object that
        //  can be created from Ctx and Message
        //  For Server it's the MessageEvent<M, ClientId>
        //  For Client it's MessageEvent<M> directly
        P::Message::add_events::<()>(app);

        app
            // PLUGINS //
            .add_plugins(SharedPlugin {
                config: config.client_config.shared.clone(),
            })
            // RESOURCES //
            .insert_resource(client)
            .init_resource::<ReplicationData>()
            // SYSTEM SETS //
            .configure_sets(PreUpdate, ClientSet::Receive)
            .configure_sets(PostUpdate, ClientSet::Send)
            // EVENTS //
            .add_event::<ConnectEvent>()
            .add_event::<DisconnectEvent>()
            .add_event::<EntitySpawnEvent>()
            // SYSTEMS //
            .add_systems(PreUpdate, receive::<P>.in_set(ClientSet::Receive))
            // TODO: a bit of a code-smell that i have to run this here instead of in the shared plugin
            //  maybe TickManager should be a separate resource not contained in Client/Server?
            //  and runs Update in PreUpdate before the client/server systems
            .add_systems(
                FixedUpdate,
                increment_tick::<Client<P>>
                    .before(ClientSet::FixedUpdateGame)
                    // run if there is no rollback resource, or if we are not in rollback
                    .run_if((not(resource_exists::<Rollback>())).or_else(not(is_in_rollback))),
            )
            .add_systems(PostUpdate, send::<P>.in_set(ClientSet::Send));
    }
}