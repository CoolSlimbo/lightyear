use anyhow::Context;
use bevy::app::PreUpdate;
use bevy::ecs::entity::MapEntities;
use std::any::TypeId;
use std::fmt::Debug;

use crate::_internal::{ReadBuffer, ReadWordBuffer, WriteBuffer, WriteWordBuffer};
use crate::client::message::add_server_to_client_message;
use crate::prelude::{client, server, Channel, RemoteEntityMap};
use bevy::prelude::{
    App, EntityMapper, EventWriter, IntoSystemConfigs, ResMut, Resource, TypePath, World,
};
use bevy::utils::HashMap;
use bitcode::encoding::Fixed;
use bitcode::{Decode, Encode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing::error;

use crate::inputs::native::input_buffer::InputMessage;
use crate::packet::message::Message;
use crate::prelude::{ChannelDirection, ChannelKind, MainSet};
use crate::protocol::component::ComponentKind;
use crate::protocol::registry::{NetId, TypeKind, TypeMapper};
use crate::protocol::{BitSerializable, EventContext};
use crate::server::message::add_client_to_server_message;
use crate::shared::replication::entity_map::EntityMap;

pub enum InputMessageKind {
    /// This is a message for a [`LeafwingUserAction`](crate::inputs::leafwing::LeafwingUserAction)
    #[cfg(feature = "leafwing")]
    Leafwing,
    /// This is a message for a [`UserAction`](crate::inputs::native::UserAction)
    Native,
    /// This is not an input message, but a regular [`Message`]
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ErasedMessageFns {
    type_id: TypeId,
    type_name: &'static str,
    // TODO: maybe use `Vec<MaybeUninit<u8>>` instead of unsafe fn(), like bevy?
    pub serialize: unsafe fn(),
    pub deserialize: unsafe fn(),
    pub map_entities: Option<unsafe fn()>,
    pub message_type: MessageType,
}

type SerializeFn<M> = fn(&M, writer: &mut WriteWordBuffer) -> anyhow::Result<()>;
type DeserializeFn<M> = fn(reader: &mut ReadWordBuffer) -> anyhow::Result<M>;
type MapEntitiesFn<M> = fn(&mut M, entity_map: &mut EntityMap);

pub struct MessageFns<M> {
    pub serialize: SerializeFn<M>,
    pub deserialize: DeserializeFn<M>,
    pub map_entities: Option<MapEntitiesFn<M>>,
    pub message_type: MessageType,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MessageType {
    /// This is a message for a [`LeafwingUserAction`](crate::inputs::leafwing::LeafwingUserAction)
    #[cfg(feature = "leafwing")]
    LeafwingInput,
    /// This is a message for a [`UserAction`](crate::inputs::native::UserAction)
    NativeInput,
    /// This is not an input message, but a regular [`Message`]
    Normal,
}

impl ErasedMessageFns {
    pub(crate) unsafe fn typed<M: Message>(&self) -> MessageFns<M> {
        debug_assert_eq!(
            self.type_id,
            TypeId::of::<M>(),
            "The erased message fns were created for type {}, but we are trying to convert to type {}",
            self.type_name,
            std::any::type_name::<M>(),
        );

        MessageFns {
            serialize: unsafe { std::mem::transmute(self.serialize) },
            deserialize: unsafe { std::mem::transmute(self.deserialize) },
            map_entities: self.map_entities.map(|m| unsafe { std::mem::transmute(m) }),
            message_type: self.message_type,
        }
    }
}

#[derive(Debug, Default, Clone, Resource, PartialEq, TypePath)]
pub struct MessageRegistry {
    // TODO: maybe instead of MessageFns, use an erased trait objects? like dyn ErasedSerialize + ErasedDeserialize ?
    //  but how do we deal with implementing behaviour for types that don't have those traits?
    fns_map: HashMap<MessageKind, ErasedMessageFns>,
    pub(crate) kind_map: TypeMapper<MessageKind>,
}

/// Add a message to the list of messages that can be sent
pub trait AppMessageExt {
    fn add_message<M: Message>(&mut self, direction: ChannelDirection);

    fn add_message_map_entities<M: MapEntities + 'static>(&mut self);
}

fn register_message_send<M: Message>(app: &mut App, direction: ChannelDirection) {
    match direction {
        ChannelDirection::ClientToServer => {
            add_client_to_server_message::<M>(app);
        }
        ChannelDirection::ServerToClient => {
            add_server_to_client_message::<M>(app);
        }
        ChannelDirection::Bidirectional => {
            register_message_send::<M>(app, ChannelDirection::ClientToServer);
            register_message_send::<M>(app, ChannelDirection::ServerToClient);
        }
    }
}

impl AppMessageExt for App {
    fn add_message<M: Message>(&mut self, direction: ChannelDirection) {
        let mut registry = self.world.resource_mut::<MessageRegistry>();
        registry.add_message::<M>(MessageType::Normal);
        register_message_send::<M>(self, direction);
    }

    // TODO: have a single map_entities function, and try on both MessageRegistry and ComponentRegistry?
    fn add_message_map_entities<M: MapEntities + 'static>(&mut self) {
        let mut registry = self.world.resource_mut::<MessageRegistry>();
        registry.add_map_entities::<M>();
    }
}

impl MessageRegistry {
    pub(crate) fn message_type(&self, net_id: NetId) -> MessageType {
        let kind = self.kind_map.kind(net_id).unwrap();
        self.fns_map
            .get(kind)
            .map(|fns| fns.message_type)
            .unwrap_or(MessageType::Normal)
    }
    pub(crate) fn add_message<M: Message>(&mut self, message_type: MessageType) {
        let message_kind = self.kind_map.add::<M>();
        let serialize: SerializeFn<M> = <M as BitSerializable>::encode;
        let deserialize: DeserializeFn<M> = <M as BitSerializable>::decode;
        self.fns_map.insert(
            message_kind,
            ErasedMessageFns {
                type_id: TypeId::of::<M>(),
                type_name: std::any::type_name::<M>(),
                serialize: unsafe { std::mem::transmute(serialize) },
                deserialize: unsafe { std::mem::transmute(deserialize) },
                map_entities: None,
                message_type,
            },
        );
    }

    pub(crate) fn add_map_entities<M: MapEntities + 'static>(&mut self) {
        let kind = MessageKind::of::<M>();
        let map_entities: MapEntitiesFn<M> = <M as MapEntities>::map_entities::<EntityMap>;
        let erased_fns = self
            .fns_map
            .get_mut(&kind)
            .expect("the message is not part of the protocol");
        erased_fns.map_entities = Some(unsafe { std::mem::transmute(map_entities) });
    }

    pub(crate) fn serialize<M: Message>(
        &self,
        message: &M,
        writer: &mut WriteWordBuffer,
    ) -> anyhow::Result<()> {
        let kind = MessageKind::of::<M>();
        let erased_fns = self
            .fns_map
            .get(&kind)
            .context("the message is not part of the protocol")?;
        let fns = unsafe { erased_fns.typed::<M>() };
        let net_id = self.kind_map.net_id(&kind).unwrap();
        writer.encode(net_id, Fixed)?;
        (fns.serialize)(message, writer)
    }

    pub(crate) fn deserialize<M: Message>(
        &self,
        reader: &mut ReadWordBuffer,
        entity_map: &mut EntityMap,
    ) -> anyhow::Result<M> {
        let net_id = reader.decode::<NetId>(Fixed)?;
        let kind = self.kind_map.kind(net_id).context("unknown message kind")?;
        let erased_fns = self
            .fns_map
            .get(kind)
            .context("the message is not part of the protocol")?;
        let fns = unsafe { erased_fns.typed::<M>() };
        let mut message = (fns.deserialize)(reader)?;
        if let Some(map_entities) = fns.map_entities {
            map_entities(&mut message, entity_map);
        }
        Ok(message)
    }

    pub(crate) fn map_entities<M: Message>(&self, message: &mut M, entity_map: &mut EntityMap) {
        let kind = MessageKind::of::<M>();
        let erased_fns = self
            .fns_map
            .get(&kind)
            .context("the message is not part of the protocol")
            .unwrap();
        let fns = unsafe { erased_fns.typed::<M>() };
        if let Some(map_entities) = fns.map_entities {
            map_entities(message, entity_map);
        }
    }
}

/// [`MessageKind`] is an internal wrapper around the type of the message
#[derive(Debug, Eq, Hash, Copy, Clone, PartialEq)]
pub struct MessageKind(TypeId);

impl MessageKind {
    pub fn of<M: 'static>() -> Self {
        Self(TypeId::of::<M>())
    }
}

impl TypeKind for MessageKind {}

impl From<TypeId> for MessageKind {
    fn from(type_id: TypeId) -> Self {
        Self(type_id)
    }
}
