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
//! use bevy::ecs::entity::MapEntities;
//! use bevy::prelude::*;
//! use lightyear::prelude::client::*;
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
//! // every input must implement MapEntities
//! impl MapEntities for MyInput {
//!     fn map_entities<M: EntityMapper>(&mut self, entity_mapper: &mut M) {
//!     }
//! }
//!
//! let mut app = App::new();
//! # app.add_plugins(ClientPlugins::new(ClientConfig::default()));
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
use tracing::{debug, error, trace};

use crate::channel::builder::InputChannel;
use crate::client::components::Confirmed;
use crate::client::config::ClientConfig;
use crate::client::connection::ConnectionManager;
use crate::client::input::{BaseInputPlugin, InputSystemSet};
use crate::client::prediction::plugin::is_in_rollback;
use crate::client::prediction::resource::PredictionManager;
use crate::client::prediction::Predicted;
use crate::inputs::native::input_buffer::InputBuffer;
use crate::inputs::native::input_message::{InputMessage, InputTarget};
use crate::inputs::native::{ActionState, InputMarker, UserAction};
use crate::prelude::{
    ChannelKind, ChannelRegistry, ClientReceiveMessage, MessageRegistry, PrePredicted, TickManager,
    TimeManager,
};
use crate::shared::input::InputConfig;
use crate::shared::tick_manager::TickEvent;

pub struct InputPlugin<A> {
    config: InputConfig<A>,
}

impl<A: UserAction> InputPlugin<A> {
    pub(crate) fn new(config: InputConfig<A>) -> Self {
        Self { config }
    }
}

impl<A: UserAction> Default for InputPlugin<A> {
    fn default() -> Self {
        Self::new(InputConfig::default())
    }
}

// TODO: is this actually necessary? The sync happens in PostUpdate,
//  so maybe it's ok if the InputMessages contain the pre-sync tick! (since those inputs happened
//  before the sync). If it's not needed, send the messages directly in FixedPostUpdate!
//  Actually maybe it is, because the send-tick on the server will be updated.
/// Buffer that will store the InputMessages we want to write this frame.
///
/// We need this because:
/// - we write the InputMessages during FixedPostUpdate
/// - we apply the TickUpdateEvents (from doing sync) during PostUpdate, which might affect the ticks from the InputMessages.
///   During this phase, we want to update the tick of the InputMessages that we wrote during FixedPostUpdate.
#[derive(Debug, Resource)]
struct MessageBuffer<A>(Vec<InputMessage<A>>);

impl<A> Default for MessageBuffer<A> {
    fn default() -> Self {
        Self(vec![])
    }
}

impl<A: UserAction> Plugin for InputPlugin<A> {
    fn build(&self, app: &mut App) {
        app.add_plugins(BaseInputPlugin::<ActionState<A>, InputMarker<A>>::default());

        // RESOURCES
        app.insert_resource(self.config.clone());
        app.init_resource::<MessageBuffer<A>>();

        // SYSTEMS
        // we don't need this for native inputs because it's handled by required components
        // app.add_observer(add_action_state::<A>);
        // app.add_observer(add_input_buffer::<A>);
        if self.config.rebroadcast_inputs {
            app.add_systems(
                RunFixedMainLoop,
                receive_remote_player_input_messages::<A>
                    .in_set(InputSystemSet::ReceiveInputMessages),
            );
        }
        app.add_systems(
            FixedPostUpdate,
            prepare_input_message::<A>
                .in_set(InputSystemSet::PrepareInputMessage)
                // no need to prepare messages to send if in rollback
                .run_if(not(is_in_rollback)),
        );
        app.add_systems(
            PostUpdate,
            send_input_messages::<A>.in_set(InputSystemSet::SendInputMessage),
        );
        // if the client tick is updated because of a desync, update the ticks in the input buffers
        app.add_observer(receive_tick_events::<A>);
    }
}

/// Take the input buffer, and prepare the input message to send to the server
fn prepare_input_message<A: UserAction>(
    connection: Res<ConnectionManager>,
    mut message_buffer: ResMut<MessageBuffer<A>>,
    channel_registry: Res<ChannelRegistry>,
    config: Res<ClientConfig>,
    input_config: Res<InputConfig<A>>,
    tick_manager: Res<TickManager>,
    input_buffer_query: Query<
        (
            Entity,
            &InputBuffer<ActionState<A>>,
            Option<&Predicted>,
            Option<&PrePredicted>,
        ),
        With<InputMarker<A>>,
    >,
) {
    // we send a message from the latest tick that we have available, which is the delayed tick
    let input_delay_ticks = connection.input_delay_ticks() as i16;
    let tick = tick_manager.tick() + input_delay_ticks;
    // TODO: the number of messages should be in SharedConfig
    trace!(delayed_tick = ?tick, current_tick = ?tick_manager.tick(), "prepare_input_message");
    // TODO: instead of redundancy, send ticks up to the latest yet ACK-ed input tick
    //  this means we would also want to track packet->message acks for unreliable channels as well, so we can notify
    //  this system what the latest acked input tick is?
    let input_send_interval = channel_registry
        .get_builder_from_kind(&ChannelKind::of::<InputChannel>())
        .unwrap()
        .settings
        .send_frequency;
    // we send redundant inputs, so that if a packet is lost, we can still recover
    // A redundancy of 2 means that we can recover from 1 lost packet
    let mut num_tick: u16 =
        ((input_send_interval.as_nanos() / config.shared.tick.tick_duration.as_nanos()) + 1)
            .try_into()
            .unwrap();
    num_tick *= input_config.packet_redundancy;
    let mut message = InputMessage::<A>::new(tick);
    for (entity, input_buffer, predicted, pre_predicted) in input_buffer_query.iter() {
        trace!(
            ?tick,
            ?entity,
            "Preparing input message with buffer: {:?}",
            input_buffer
        );

        // Make sure that server can read the inputs correctly
        // TODO: currently we are not sending inputs for pre-predicted entities until we receive the confirmation from the server
        //  could we find a way to do it?
        //  maybe if it's pre-predicted, we send the original entity (pre-predicted), and the server will apply the conversion
        //   on their end?
        if pre_predicted.is_some() {
            // wait until the client receives the PrePredicted entity confirmation to send inputs
            // otherwise we get failed entity_map logs
            // TODO: the problem is that we wait until we have received the server answer. Ideally we would like
            //  to wait until the server has received the PrePredicted entity
            if predicted.is_none() {
                continue;
            }
            trace!(
                ?tick,
                "sending inputs for pre-predicted entity! Local client entity: {:?}",
                entity
            );
            // TODO: not sure if this whole pre-predicted inputs thing is worth it, because the server won't be able to
            //  to receive the inputs until it receives the pre-predicted spawn message.
            //  so all the inputs sent between pre-predicted spawn and server-receives-pre-predicted will be lost

            // TODO: I feel like pre-predicted inputs work well only for global-inputs, because then the server can know
            //  for which client the inputs were!

            // 0. the entity is pre-predicted, no need to convert the entity (the mapping will be done on the server, when
            // receiving the message. It's possible because the server received the PrePredicted entity before)
            message.add_inputs(
                num_tick,
                InputTarget::PrePredictedEntity(entity),
                input_buffer,
            );
        } else {
            // 1. if the entity is confirmed, we need to convert the entity to the server's entity
            // 2. if the entity is predicted, we need to first convert the entity to confirmed, and then from confirmed to remote
            if let Some(confirmed) = predicted.map_or(Some(entity), |p| p.confirmed_entity) {
                if let Some(server_entity) = connection
                    .replication_receiver
                    .remote_entity_map
                    .get_remote(confirmed)
                {
                    trace!("sending input for server entity: {:?}. local entity: {:?}, confirmed: {:?}", server_entity, entity, confirmed);
                    // println!(
                    //     "preparing input message using input_buffer: {}",
                    //     input_buffer
                    // );
                    message.add_inputs(num_tick, InputTarget::Entity(server_entity), input_buffer);
                }
            } else {
                // TODO: entity is not predicted or not confirmed? also need to do the conversion, no?
                trace!("not sending inputs because couldnt find server entity");
            }
        }
    }

    // we send a message even when there are 0 inputs because that itself is information
    trace!(
        ?tick,
        ?num_tick,
        "sending input message for {:?}: {:?}",
        core::any::type_name::<A>(),
        message
    );
    message_buffer.0.push(message);

    // NOTE: keep the older input values in the InputBuffer! because they might be needed when we rollback for client prediction
}

/// Read the InputMessages of other clients from the server to update their InputBuffer and ActionState.
/// This is useful if we want to do client-prediction for remote players.
///
/// If the InputBuffer/ActionState is missing, we will add it.
///
/// We will apply the diffs on the Predicted entity.
fn receive_remote_player_input_messages<A: UserAction>(
    mut commands: Commands,
    tick_manager: Res<TickManager>,
    mut received_inputs: ResMut<Events<ClientReceiveMessage<InputMessage<A>>>>,
    connection: Res<ConnectionManager>,
    prediction_manager: Res<PredictionManager>,
    message_registry: Res<MessageRegistry>,
    // TODO: currently we do not handle entities that are controlled by multiple clients
    confirmed_query: Query<&Confirmed, Without<InputMarker<A>>>,
    mut predicted_query: Query<
        Option<&mut InputBuffer<ActionState<A>>>,
        (With<Predicted>, Without<InputMarker<A>>),
    >,
) {
    let tick = tick_manager.tick();
    received_inputs.drain().for_each(|event| {
        let message = event.message;
        trace!(?message.end_tick, %message, "received remote input message for action: {:?}", core::any::type_name::<A>());
        for target_data in &message.inputs {
            // - the input target has already been set to the server entity in the InputMessage
            // - it has been mapped to a client-entity on the client during deserialization
            //   ONLY if it's PrePredicted (look at the MapEntities implementation)
            let entity = match target_data.target {
                InputTarget::Entity(entity) => {
                    // TODO: find a better way!
                    // if InputTarget = Entity, we still need to do the mapping
                    connection
                        .replication_receiver
                        .remote_entity_map
                        .get_local(entity)
                }
                InputTarget::PrePredictedEntity(entity) => Some(entity),
            };
            if let Some(entity) = entity {
                debug!(
                    "received input message for entity: {:?}. Applying to diff buffer.",
                    entity
                );
                if let Ok(confirmed) = confirmed_query.get(entity) {
                    if let Some(predicted) = confirmed.predicted {
                        if let Ok(input_buffer) = predicted_query.get_mut(predicted) {
                            trace!(confirmed= ?entity, ?predicted, end_tick = ?message.end_tick, "update action diff buffer for remote player PREDICTED using input message");
                            if let Some(mut input_buffer) = input_buffer {
                                input_buffer.update_from_message(message.end_tick, &target_data.states);
                                #[cfg(feature = "metrics")]
                                {
                                    let margin = input_buffer.end_tick().unwrap() - tick;
                                    metrics::gauge!(format!(
                                                    "inputs::{}::remote_player::{}::buffer_margin",
                                                    core::any::type_name::<A>(),
                                                    entity
                                                ))
                                        .set(margin as f64);
                                    metrics::gauge!(format!(
                                                    "inputs::{}::remote_player::{}::buffer_size",
                                                    core::any::type_name::<A>(),
                                                    entity
                                                ))
                                        .set(input_buffer.len() as f64);
                                }
                            } else {
                                // add the ActionState or InputBuffer if they are missing
                                let mut input_buffer = InputBuffer::<ActionState<A>>::default();
                                input_buffer.update_from_message(
                                    message.end_tick,
                                    &target_data.states,
                                );
                                // if the remote_player's predicted entity doesn't have the InputBuffer, we need to insert them
                                commands.entity(predicted).insert((
                                    input_buffer,
                                    ActionState::<A>::default(),
                                ));
                            };
                        }
                    }
                } else {
                    error!(?entity, ?target_data.states, end_tick = ?message.end_tick, "received input message for unrecognized entity");
                }
            } else {
                error!("received remote player input message for unrecognized entity");
            }
        }
    });
}

/// Drain the messages from the buffer and send them to the server
fn send_input_messages<A: UserAction>(
    mut connection: ResMut<ConnectionManager>,
    input_config: Res<InputConfig<A>>,
    mut message_buffer: ResMut<MessageBuffer<A>>,
    time_manager: Res<TimeManager>,
    tick_manager: Res<TickManager>,
) {
    trace!(
        "Number of input messages to send: {:?}",
        message_buffer.0.len()
    );
    for mut message in message_buffer.0.drain(..) {
        // if lag compensation is enabled, we send the current delay to the server
        // (this runs here because the delay is only correct after the SyncSet has run)
        // TODO: or should we actually use the interpolation_delay BEFORE SyncSet
        //  because the user is reacting to stuff from the previous frame?
        if input_config.lag_compensation {
            message.interpolation_delay = Some(
                connection
                    .sync_manager
                    .interpolation_delay(tick_manager.as_ref(), time_manager.as_ref()),
            );
        }
        connection
            .send_message::<InputChannel, InputMessage<A>>(&message)
            .unwrap_or_else(|err| {
                error!("Error while sending input message: {:?}", err);
            });
    }
}

/// In case the client tick changes suddenly, we also update the InputBuffer accordingly
fn receive_tick_events<A: UserAction>(
    trigger: Trigger<TickEvent>,
    mut message_buffer: ResMut<MessageBuffer<A>>,
    mut input_buffer_query: Query<&mut InputBuffer<ActionState<A>>>,
) {
    match *trigger.event() {
        TickEvent::TickSnap { old_tick, new_tick } => {
            for mut input_buffer in input_buffer_query.iter_mut() {
                if let Some(start_tick) = input_buffer.start_tick {
                    input_buffer.start_tick = Some(start_tick + (new_tick - old_tick));
                    debug!(
                        "Receive tick snap event {:?}. Updating input buffer start_tick to {:?}!",
                        trigger.event(),
                        input_buffer.start_tick
                    );
                }
            }
            for message in message_buffer.0.iter_mut() {
                message.end_tick = message.end_tick + (new_tick - old_tick);
            }
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use crate::client::input::native::InputSystemSet;
//     use crate::prelude::client::InputManager;
//     use crate::prelude::{server, TickManager};
//     use crate::tests::host_server_stepper::HostServerStepper;
//     use crate::tests::protocol::MyInput;
//     use bevy::prelude::*;
//
//     fn press_input(
//         mut input_manager: ResMut<InputManager<MyInput>>,
//         tick_manager: Res<TickManager>,
//     ) {
//         input_manager.add_input(MyInput(2), tick_manager.tick());
//     }
//
//     #[derive(Resource)]
//     pub struct Counter(pub u32);
//
//     fn receive_input(
//         mut counter: ResMut<Counter>,
//         mut input: EventReader<server::InputEvent<MyInput>>,
//     ) {
//         for input in input.read() {
//             assert_eq!(input.input().unwrap(), MyInput(2));
//             counter.0 += 1;
//         }
//     }
//
//     /// Check that in host-server mode the native client inputs from the buffer
//     /// are forwarded directly to the server's InputEvents
//     #[test]
//     fn test_host_server_input() {
//         let mut stepper = HostServerStepper::default_no_init();
//         stepper.server_app.world_mut().insert_resource(Counter(0));
//         stepper.server_app.add_systems(
//             FixedPreUpdate,
//             press_input.in_set(InputSystemSet::BufferInputs),
//         );
//         stepper.server_app.add_systems(FixedUpdate, receive_input);
//         stepper.init();
//
//         stepper.frame_step();
//         assert!(stepper.server_app.world().resource::<Counter>().0 > 0);
//     }
// }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::server::{Replicate, SyncTarget};
    use crate::prelude::{client, NetworkTarget};
    use crate::tests::host_server_stepper::HostServerStepper;
    use crate::tests::protocol::MyInput;

    // Test with no input delay:
    // 1. remote client replicated entity sending inputs to server
    // 2. remote client predicted entity sending inputs to server
    // 3. remote client confirmed entity sending inputs to server
    // 4. remote client pre-predicted entity sending inputs to server
    // 5. local client sending inputs to server
    // 6. local client sending inputs to remote client (for prediction)
    #[test]
    fn test_host_server_input() {
        // tracing_subscriber::FmtSubscriber::builder()
        //     .with_max_level(tracing::Level::ERROR)
        //     .init();
        let mut stepper = HostServerStepper::default();

        // SETUP START
        // entity controlled by the local client
        let local_entity = stepper
            .server_app
            .world_mut()
            .spawn((
                Replicate {
                    sync: SyncTarget {
                        prediction: NetworkTarget::All,
                        ..default()
                    },
                    ..default()
                },
                InputMarker::<MyInput>::default(),
            ))
            .id();
        // entity controlled by the remote client
        let remote_entity = stepper
            .server_app
            .world_mut()
            .spawn(Replicate::default())
            .id();
        let remote_entity_2 = stepper
            .server_app
            .world_mut()
            .spawn(Replicate {
                sync: SyncTarget {
                    prediction: NetworkTarget::All,
                    ..default()
                },
                ..default()
            })
            .id();
        let remote_entity_3 = stepper
            .server_app
            .world_mut()
            .spawn(Replicate {
                sync: SyncTarget {
                    prediction: NetworkTarget::All,
                    ..default()
                },
                ..default()
            })
            .id();
        let client_pre_predicted_entity = stepper
            .client_app
            .world_mut()
            .spawn((client::Replicate::default(), PrePredicted::default()))
            .id();
        for _ in 0..10 {
            stepper.frame_step();
        }

        let local_confirmed = stepper
            .client_app
            .world()
            .resource::<client::ConnectionManager>()
            .replication_receiver
            .remote_entity_map
            .get_local(local_entity)
            .expect("entity was not replicated to client");
        let local_predicted = stepper
            .client_app
            .world()
            .get::<Confirmed>(local_confirmed)
            .unwrap()
            .predicted
            .unwrap();
        let client_entity = stepper
            .client_app
            .world()
            .resource::<client::ConnectionManager>()
            .replication_receiver
            .remote_entity_map
            .get_local(remote_entity)
            .expect("entity was not replicated to client");
        let client_entity_2_confirmed = stepper
            .client_app
            .world()
            .resource::<client::ConnectionManager>()
            .replication_receiver
            .remote_entity_map
            .get_local(remote_entity_2)
            .expect("entity was not replicated to client");
        let client_entity_2_predicted = stepper
            .client_app
            .world()
            .get::<Confirmed>(client_entity_2_confirmed)
            .unwrap()
            .predicted
            .unwrap();
        let client_entity_3_confirmed = stepper
            .client_app
            .world()
            .resource::<client::ConnectionManager>()
            .replication_receiver
            .remote_entity_map
            .get_local(remote_entity_3)
            .expect("entity was not replicated to client");
        let server_pre_predicted_entity = stepper
            .server_app
            .world_mut()
            .query_filtered::<Entity, With<PrePredicted>>()
            .single(stepper.server_app.world())
            .unwrap();
        // replicate back the pre-predicted entity
        stepper
            .server_app
            .world_mut()
            .entity_mut(server_pre_predicted_entity)
            .insert(Replicate::default());
        stepper.frame_step();
        stepper.frame_step();
        // SETUP END

        // 1. remote client replicated entity send to server
        stepper
            .client_app
            .world_mut()
            .entity_mut(client_entity)
            .insert(InputMarker::<MyInput>::default());
        stepper
            .client_app
            .world_mut()
            .get_mut::<ActionState<MyInput>>(client_entity)
            .unwrap()
            .value = Some(MyInput(1));
        stepper.frame_step();
        let server_tick = stepper.server_tick();
        let client_tick = stepper.client_tick();

        assert_eq!(
            stepper
                .server_app
                .world()
                .get::<InputBuffer<ActionState<MyInput>>>(remote_entity)
                .unwrap()
                .get(client_tick)
                .unwrap(),
            &ActionState {
                value: Some(MyInput(1))
            }
        );

        // we want to advance by the tick difference, so that the server is on the same
        // tick as when the client sent the input
        for tick in (server_tick.0 as usize)..(client_tick.0 as usize) {
            stepper.frame_step();
        }
        assert_eq!(
            stepper
                .server_app
                .world()
                .get::<ActionState<MyInput>>(remote_entity)
                .unwrap(),
            &ActionState {
                value: Some(MyInput(1))
            }
        );

        // 2. remote client predicted entity send inputs to server
        stepper
            .client_app
            .world_mut()
            .entity_mut(client_entity_2_predicted)
            .insert(InputMarker::<MyInput>::default());
        stepper
            .client_app
            .world_mut()
            .get_mut::<ActionState<MyInput>>(client_entity_2_predicted)
            .unwrap()
            .value = Some(MyInput(2));
        stepper.frame_step();
        let server_tick = stepper.server_tick();
        let client_tick = stepper.client_tick();

        assert_eq!(
            stepper
                .server_app
                .world()
                .get::<InputBuffer<ActionState<MyInput>>>(remote_entity_2)
                .unwrap()
                .get(client_tick)
                .unwrap(),
            &ActionState {
                value: Some(MyInput(2))
            }
        );

        // we want to advance by the tick difference, so that the server is on the same
        // tick as when the client sent the input
        for tick in (server_tick.0 as usize)..(client_tick.0 as usize) {
            stepper.frame_step();
        }
        assert_eq!(
            stepper
                .server_app
                .world()
                .get::<ActionState<MyInput>>(remote_entity_2)
                .unwrap(),
            &ActionState {
                value: Some(MyInput(2))
            }
        );

        // 3. remote client confirmed entity send inputs to server
        stepper
            .client_app
            .world_mut()
            .entity_mut(client_entity_3_confirmed)
            .insert(InputMarker::<MyInput>::default());
        stepper
            .client_app
            .world_mut()
            .get_mut::<ActionState<MyInput>>(client_entity_3_confirmed)
            .unwrap()
            .value = Some(MyInput(3));
        stepper.frame_step();
        let server_tick = stepper.server_tick();
        let client_tick = stepper.client_tick();

        assert_eq!(
            stepper
                .server_app
                .world()
                .get::<InputBuffer<ActionState<MyInput>>>(remote_entity_3)
                .unwrap()
                .get(client_tick)
                .unwrap(),
            &ActionState {
                value: Some(MyInput(3))
            }
        );

        // we want to advance by the tick difference, so that the server is on the same
        // tick as when the client sent the input
        for tick in (server_tick.0 as usize)..(client_tick.0 as usize) {
            stepper.frame_step();
        }
        assert_eq!(
            stepper
                .server_app
                .world()
                .get::<ActionState<MyInput>>(remote_entity_3)
                .unwrap(),
            &ActionState {
                value: Some(MyInput(3))
            }
        );

        // 4. remote client pre-predicted entity send inputs to server
        stepper
            .client_app
            .world_mut()
            .entity_mut(client_pre_predicted_entity)
            .insert(InputMarker::<MyInput>::default());
        stepper
            .client_app
            .world_mut()
            .get_mut::<ActionState<MyInput>>(client_pre_predicted_entity)
            .unwrap()
            .value = Some(MyInput(4));
        stepper.frame_step();
        let server_tick = stepper.server_tick();
        let client_tick = stepper.client_tick();

        assert_eq!(
            stepper
                .server_app
                .world()
                .get::<InputBuffer<ActionState<MyInput>>>(server_pre_predicted_entity)
                .unwrap()
                .get(client_tick)
                .unwrap(),
            &ActionState {
                value: Some(MyInput(4))
            }
        );

        // we want to advance by the tick difference, so that the server is on the same
        // tick as when the client sent the input
        for tick in (server_tick.0 as usize)..(client_tick.0 as usize) {
            stepper.frame_step();
        }
        assert_eq!(
            stepper
                .server_app
                .world()
                .get::<ActionState<MyInput>>(server_pre_predicted_entity)
                .unwrap(),
            &ActionState {
                value: Some(MyInput(4))
            }
        );

        // 5. local client inputs sent to server
        // we get this for free because the ActionState is updated in InputSystemSet::WriteClientInputs
        // which runs in host-server mode

        // 6. local host-server client inputs sent to remote client for prediction
        // i.e. the host-server inputs are being broadcasted to other clients
        stepper
            .server_app
            .world_mut()
            .get_mut::<ActionState<MyInput>>(local_entity)
            .unwrap()
            .value = Some(MyInput(6));
        // we run server first, then client, so that the server's rebroadcasted inputs can be read by the client
        stepper.advance_time(stepper.frame_duration);
        stepper.server_app.update();
        stepper.client_app.update();
        let server_tick = stepper.server_tick();
        let client_tick = stepper.client_tick();

        // for input broadcasting, we write the remote client inputs to the Predicted entity only
        assert_eq!(
            stepper
                .client_app
                .world()
                .get::<InputBuffer<ActionState<MyInput>>>(local_predicted)
                .unwrap()
                .get(server_tick)
                .unwrap(),
            &ActionState {
                value: Some(MyInput(6))
            }
        );
    }
}
