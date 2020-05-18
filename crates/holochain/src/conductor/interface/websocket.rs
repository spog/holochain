use super::error::{InterfaceError, InterfaceResult};
use crate::conductor::{
    conductor::StopReceiver,
    interface::*,
    manager::{ManagedTaskHandle, ManagedTaskResult},
};
use crate::core::signal::Signal;
use holochain_serialized_bytes::SerializedBytes;
use holochain_websocket::{
    websocket_bind, WebsocketConfig, WebsocketListener, WebsocketMessage, WebsocketReceiver,
    WebsocketSender,
};
use std::convert::TryFrom;

use std::sync::Arc;
use tokio::stream::StreamExt;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::*;
use url2::url2;

// TODO: This is arbitrary, choose reasonable size.
/// Number of singals in buffer before applying
/// back pressure.
pub(crate) const SIGNAL_BUFFER_SIZE: usize = 1000;

/// Create an Admin Interface, which only receives AdminRequest messages
/// from the external client
pub async fn spawn_websocket_listener(port: u16) -> InterfaceResult<WebsocketListener> {
    trace!("Initializing Admin interface");
    let listener = websocket_bind(
        url2!("ws://127.0.0.1:{}", port),
        Arc::new(WebsocketConfig::default()),
    )
    .await?;
    trace!("LISTENING AT: {}", listener.local_addr());
    Ok(listener)
}

pub fn spawn_admin_interface_task<A: InterfaceApi>(
    mut listener: WebsocketListener,
    api: A,
    mut stop_rx: StopReceiver,
) -> InterfaceResult<ManagedTaskHandle> {
    Ok(tokio::task::spawn(async move {
        let mut listener_handles = Vec::new();
        let mut send_sockets = Vec::new();
        loop {
            tokio::select! {
                // break if we receive on the stop channel
                _ = stop_rx.recv() => { break; },

                // establish a new connection to a client
                maybe_con = listener.next() => if let Some(connection) = maybe_con {
                    match connection {
                        Ok((send_socket, recv_socket)) => {
                            send_sockets.push(send_socket);
                            listener_handles.push(tokio::task::spawn(recv_incoming_admin_msgs(
                                api.clone(),
                                recv_socket,
                            )));
                        }
                        Err(err) => {
                            warn!("Admin socket connection failed: {}", err);
                        }
                    }
                } else {
                    warn!(line = line!(), "Listener has returned none");
                    // This shouldn't actually ever happen, but if it did,
                    // we would just stop the listener task
                    break;
                }
            }
        }
        // TODO: TK-01261: drop listener, make sure all these tasks finish!
        drop(listener);

        // TODO: TK-01261: Make send_socket close tell the recv socket to close locally in the websocket code
        for mut send_socket in send_sockets {
            // TODO: TK-01261: change from u16 code to enum
            send_socket.close(1000, "Shutting down".into()).await?;
        }

        // These SHOULD end soon after we get here, or by the time we get here.
        for h in listener_handles {
            // Show if these are actually finishing
            match tokio::time::timeout(std::time::Duration::from_secs(1), h).await {
                Ok(r) => r?,
                Err(_) => warn!("Websocket listener failed to join child tasks"),
            }
        }
        ManagedTaskResult::Ok(())
    }))
}

/// Create an App Interface, which includes the ability to receive signals
/// from Cells via a broadcast channel
pub async fn spawn_app_interface_task<A: InterfaceApi>(
    port: u16,
    api: A,
    signal_broadcaster: broadcast::Sender<Signal>,
    mut stop_rx: StopReceiver,
) -> InterfaceResult<(u16, ManagedTaskHandle)> {
    trace!("Initializing App interface");
    let mut listener = websocket_bind(
        url2!("ws://127.0.0.1:{}", port),
        Arc::new(WebsocketConfig::default()),
    )
    .await?;
    trace!("LISTENING AT: {}", listener.local_addr());
    let port = listener
        .local_addr()
        .port()
        .ok_or(InterfaceError::PortError)?;
    let task = tokio::task::spawn(async move {
        let mut listener_handles = Vec::new();

        let mut handle_connection =
            |send_socket: WebsocketSender, recv_socket: WebsocketReceiver| {
                let signal_rx = signal_broadcaster.subscribe();
                listener_handles.push(tokio::task::spawn(recv_incoming_msgs_and_outgoing_signals(
                    api.clone(),
                    recv_socket,
                    signal_rx,
                    send_socket,
                )));
            };

        loop {
            tokio::select! {
                // break if we receive on the stop channel
                _ = stop_rx.recv() => { break; },

                // establish a new connection to a client
                maybe_con = listener.next() => if let Some(connection) = maybe_con {
                    match connection {
                        Ok((send_socket, recv_socket)) => {
                            handle_connection(send_socket, recv_socket);
                        }
                        Err(err) => {
                            warn!("Admin socket connection failed: {}", err);
                        }
                    }
                } else {
                    break;
                }
            }
        }

        handle_shutdown(listener_handles).await;
        ManagedTaskResult::Ok(())
    });
    Ok((port, task))
}

async fn handle_shutdown(listener_handles: Vec<JoinHandle<InterfaceResult<()>>>) {
    for h in listener_handles {
        // Show if these are actually finishing
        match tokio::time::timeout(std::time::Duration::from_secs(1), h).await {
            Ok(Ok(Ok(_))) => (),
            r @ _ => warn!(message = "Websocket listener failed to join child tasks", result = ?r),
        }
    }
}

/// Polls for messages coming in from the external client.
/// Used by Admin interface.
async fn recv_incoming_admin_msgs<A: InterfaceApi>(api: A, mut recv_socket: WebsocketReceiver) {
    while let Some(msg) = recv_socket.next().await {
        match handle_incoming_message(msg, api.clone()).await {
            Err(InterfaceError::Closed) => break,
            Err(e) => error!(error = &e as &dyn std::error::Error),
            Ok(()) => (),
        }
    }
}

/// Polls for messages coming in from the external client while simultaneously
/// polling for signals being broadcast from the Cells associated with this
/// App interface.
async fn recv_incoming_msgs_and_outgoing_signals<A: InterfaceApi>(
    api: A,
    mut recv_socket: WebsocketReceiver,
    mut signal_rx: broadcast::Receiver<Signal>,
    mut signal_tx: WebsocketSender,
) -> InterfaceResult<()> {
    trace!("CONNECTION: {}", recv_socket.remote_addr());

    loop {
        tokio::select! {
            // If we receive a Signal broadcasted from a Cell, push it out
            // across the interface
            signal = signal_rx.next() => {
                if let Some(signal) = signal {
                    let bytes = SerializedBytes::try_from(
                        signal.map_err(InterfaceError::SignalReceive)?,
                    )?;
                    signal_tx.signal(bytes).await?;
                } else {
                    debug!("Closing interface: signal stream empty");
                    break;
                }
            },

            // If we receive a message from outside, handle it
            msg = recv_socket.next() => {
                if let Some(msg) = msg {
                    handle_incoming_message(msg, api.clone()).await?
                } else {
                    debug!("Closing interface: message stream empty");
                    break;
                }
            },
        }
    }

    Ok(())
}

/// Handles messages on all interfaces
async fn handle_incoming_message<A>(ws_msg: WebsocketMessage, api: A) -> InterfaceResult<()>
where
    A: InterfaceApi,
{
    match ws_msg {
        WebsocketMessage::Request(bytes, respond) => {
            Ok(respond(api.handle_request(bytes.try_into()).await?.try_into()?).await?)
        }
        WebsocketMessage::Signal(msg) => {
            error!(msg = ?msg, "Got an unexpected Signal while handing incoming message");
            Ok(())
        }
        WebsocketMessage::Close(_) => Err(InterfaceError::Closed),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::conductor::{
        api::{error::ExternalApiWireError, AdminRequest, AdminResponse, RealAdminInterfaceApi},
        conductor::ConductorBuilder,
        dna_store::{error::DnaStoreError, MockDnaStore},
        state::ConductorState,
        Conductor, ConductorHandle,
    };
    use crate::core::{
        ribosome::wasm_test::zome_invocation_from_names,
        state::source_chain::{SourceChain, SourceChainBuf},
    };
    use futures::future::FutureExt;
    use holochain_serialized_bytes::prelude::*;
    use holochain_state::{
        buffer::BufferedStore,
        env::{EnvironmentWrite, ReadManager, WriteManager},
        test_utils::{test_conductor_env, test_wasm_env, TestEnvironment},
    };
    use holochain_types::{
        cell::CellId,
        observability,
        test_utils::{fake_agent_pubkey_1, fake_dna_file, fake_dna_zomes, write_fake_dna_file},
    };
    use holochain_wasm_test_utils::TestWasm;
    use holochain_websocket::WebsocketMessage;
    use matches::assert_matches;
    use mockall::predicate;
    use std::{collections::HashMap, convert::TryInto};
    use tempdir::TempDir;
    use uuid::Uuid;

    #[derive(Debug, serde::Serialize, serde::Deserialize, SerializedBytes)]
    #[serde(rename = "snake-case", tag = "type", content = "data")]
    enum AdmonRequest {
        InstallsDna(String),
    }

    async fn fake_genesis(env: EnvironmentWrite) {
        let env_ref = env.guard().await;
        let reader = env_ref.reader().unwrap();

        let mut source_chain = SourceChain::new(&reader, &env).unwrap();
        crate::core::workflow::fake_genesis(&mut source_chain).await;

        // Flush the db
        env_ref
            .with_commit(|writer| source_chain.0.flush_to_txn(writer))
            .unwrap();
    }

    async fn setup_admin_cells(dna_store: MockDnaStore) -> (Arc<TempDir>, ConductorHandle) {
        let test_env = test_conductor_env();
        let TestEnvironment {
            env: wasm_env,
            tmpdir: _tmpdir,
        } = test_wasm_env();
        let tmpdir = test_env.tmpdir.clone();
        let conductor_handle = ConductorBuilder::with_mock_dna_store(dna_store)
            .test(test_env, wasm_env)
            .await
            .unwrap();
        (tmpdir, conductor_handle)
    }

    async fn setup_admin() -> (Arc<TempDir>, RealAdminInterfaceApi) {
        let test_env = test_conductor_env();
        let TestEnvironment {
            env: wasm_env,
            tmpdir: _tmpdir,
        } = test_wasm_env();
        let tmpdir = test_env.tmpdir.clone();
        let conductor_handle = Conductor::builder().test(test_env, wasm_env).await.unwrap();
        (tmpdir, RealAdminInterfaceApi::new(conductor_handle))
    }

    async fn setup_admin_fake_cells(
        cell_ids: &[CellId],
        dna_store: MockDnaStore,
    ) -> (Vec<Arc<TempDir>>, ConductorHandle) {
        let mut tmps = vec![];
        let test_env = test_conductor_env();
        let TestEnvironment {
            env: wasm_env,
            tmpdir,
        } = test_wasm_env();
        tmps.push(tmpdir);
        tmps.push(test_env.tmpdir.clone());
        let mut state = ConductorState::default();
        for cell in cell_ids {
            state.cell_ids_with_proofs.push((cell.clone(), None));
        }
        let conductor_handle = ConductorBuilder::with_mock_dna_store(dna_store)
            .fake_state(state)
            .test(test_env, wasm_env)
            .await
            .unwrap();
        (tmps, conductor_handle)
    }

    async fn setup_app(
        cell_id: CellId,
        dna_store: MockDnaStore,
    ) -> (Arc<TempDir>, RealAppInterfaceApi) {
        let test_env = test_conductor_env();
        let TestEnvironment {
            env: wasm_env,
            tmpdir: _tmpdir,
        } = test_wasm_env();
        let tmpdir = test_env.tmpdir.clone();
        let mut state = ConductorState::default();
        state.cell_ids_with_proofs.push((cell_id.clone(), None));

        let conductor_handle = ConductorBuilder::with_mock_dna_store(dna_store)
            .fake_state(state)
            .test(test_env, wasm_env)
            .await
            .unwrap();

        let cell_env = conductor_handle.get_cell_env(&cell_id).await.unwrap();
        fake_genesis(cell_env).await;

        (tmpdir, RealAppInterfaceApi::new(conductor_handle))
    }

    #[tokio::test(threaded_scheduler)]
    async fn serialization_failure() {
        let (_tmpdir, admin_api) = setup_admin().await;
        let msg = AdmonRequest::InstallsDna("".into());
        let msg = msg.try_into().unwrap();
        let respond = |bytes: SerializedBytes| {
            let response: AdminResponse = bytes.try_into().unwrap();
            assert_matches!(
                response,
                AdminResponse::Error(ExternalApiWireError::Deserialization(_))
            );
            async { Ok(()) }.boxed()
        };
        let respond = Box::new(respond);
        let msg = WebsocketMessage::Request(msg, respond);
        handle_incoming_message(msg, admin_api).await.unwrap();
    }

    #[tokio::test(threaded_scheduler)]
    async fn invalid_request() {
        observability::test_run().ok();
        let (_tmpdir, admin_api) = setup_admin().await;
        let msg = AdminRequest::InstallDna("some$\\//weird00=-+[] \\Path".into(), None);
        let msg = msg.try_into().unwrap();
        let respond = |bytes: SerializedBytes| {
            let response: AdminResponse = bytes.try_into().unwrap();
            assert_matches!(
                response,
                AdminResponse::Error(ExternalApiWireError::DnaReadError(_))
            );
            async { Ok(()) }.boxed()
        };
        let respond = Box::new(respond);
        let msg = WebsocketMessage::Request(msg, respond);
        handle_incoming_message(msg, admin_api).await.unwrap()
    }

    #[tokio::test(threaded_scheduler)]
    async fn cache_failure() {
        let test_env = test_conductor_env();
        let TestEnvironment {
            env: wasm_env,
            tmpdir: _tmpdir,
        } = test_wasm_env();
        let _tmpdir = test_env.tmpdir.clone();

        let uuid = Uuid::new_v4();
        let dna = fake_dna_file(&uuid.to_string());

        let (fake_dna_path, _tmpdir) = write_fake_dna_file(dna.clone()).await.unwrap();
        let mut dna_cache = MockDnaStore::new();
        dna_cache
            .expect_add()
            .with(predicate::eq(dna))
            .returning(|_| Err(DnaStoreError::WriteFail));

        let conductor_handle = ConductorBuilder::with_mock_dna_store(dna_cache)
            .test(test_env, wasm_env)
            .await
            .unwrap();
        let admin_api = RealAdminInterfaceApi::new(conductor_handle);
        let msg = AdminRequest::InstallDna(fake_dna_path, None);
        let msg = msg.try_into().unwrap();
        let respond = |bytes: SerializedBytes| {
            let response: AdminResponse = bytes.try_into().unwrap();
            assert_matches!(
                response,
                AdminResponse::Error(ExternalApiWireError::InternalError(_))
            );
            async { Ok(()) }.boxed()
        };
        let respond = Box::new(respond);
        let msg = WebsocketMessage::Request(msg, respond);
        handle_incoming_message(msg, admin_api).await.unwrap()
    }

    #[ignore]
    #[tokio::test(threaded_scheduler)]
    async fn deserialization_failure() {
        // TODO: B-01440: this can't be done easily yet
        // because we can't serialize something that
        // doesn't deserialize
    }

    #[tokio::test(threaded_scheduler)]
    async fn call_zome_function() {
        observability::test_run().ok();
        #[derive(Debug, serde::Serialize, serde::Deserialize, SerializedBytes)]
        struct Payload {
            a: u32,
        }
        let uuid = Uuid::new_v4();
        let dna = fake_dna_zomes(
            &uuid.to_string(),
            vec![("zomey".into(), TestWasm::Foo.into())],
        );
        let payload = Payload { a: 1 };
        let dna_hash = dna.dna_hash().clone();
        let cell_id = CellId::from((dna_hash.clone(), fake_agent_pubkey_1()));

        let mut dna_store = MockDnaStore::new();

        dna_store
            .expect_get()
            .with(predicate::eq(dna_hash))
            .returning(move |_| Some(dna.clone()));

        let (_tmpdir, app_api) = setup_app(cell_id.clone(), dna_store).await;
        let mut request = Box::new(zome_invocation_from_names(
            "zomey",
            "foo",
            payload.try_into().unwrap(),
        ));
        request.cell_id = cell_id;
        let msg = AppRequest::ZomeInvocationRequest { request };
        let msg = msg.try_into().unwrap();
        let respond = |bytes: SerializedBytes| {
            let response: AppResponse = bytes.try_into().unwrap();
            assert_matches!(response, AppResponse::ZomeInvocationResponse{ .. });
            async { Ok(()) }.boxed()
        };
        let respond = Box::new(respond);
        let msg = WebsocketMessage::Request(msg, respond);
        handle_incoming_message(msg, app_api).await.unwrap();
    }

    #[tokio::test(threaded_scheduler)]
    async fn activate_app() {
        observability::test_run().ok();
        let dnas = [Uuid::new_v4(); 2]
            .iter()
            .map(|uuid| fake_dna_file(&uuid.to_string()))
            .collect::<Vec<_>>();
        let dna_map = dnas
            .iter()
            .cloned()
            .map(|dna| (dna.dna_hash().clone(), dna))
            .collect::<HashMap<_, _>>();
        let dna_hashes = dna_map
            .keys()
            .cloned()
            .map(|hash| (hash, None))
            .collect::<Vec<_>>();
        let mut dna_store = MockDnaStore::new();
        dna_store
            .expect_get()
            .returning(move |hash| dna_map.get(&hash).cloned());
        let (_tmpdir, handle) = setup_admin_cells(dna_store).await;

        let agent_key = fake_agent_pubkey_1();
        let msg = AdminRequest::ActivateApp {
            hashes_with_proofs: dna_hashes.clone(),
            agent_key: agent_key.clone(),
        };
        let msg = msg.try_into().unwrap();
        let respond = |bytes: SerializedBytes| {
            let response: AdminResponse = bytes.try_into().unwrap();
            assert_matches!(response, AdminResponse::AppsActivated);
            async { Ok(()) }.boxed()
        };
        let respond = Box::new(respond);
        let msg = WebsocketMessage::Request(msg, respond);
        handle_incoming_message(msg, RealAdminInterfaceApi::new(handle.clone()))
            .await
            .unwrap();
        let cells = handle
            .get_state_from_handle()
            .await
            .unwrap()
            .cell_ids_with_proofs;
        let expected = dna_hashes
            .into_iter()
            .map(|(hash, proof)| (CellId::from((hash, agent_key.clone())), proof))
            .collect::<Vec<_>>();
        assert_eq!(expected, cells);
    }

    #[tokio::test(threaded_scheduler)]
    async fn attach_app_interface() {
        observability::test_run().ok();
        let (_tmpdir, admin_api) = setup_admin().await;
        let msg = AdminRequest::AttachAppInterface { port: None };
        let msg = msg.try_into().unwrap();
        let respond = |bytes: SerializedBytes| {
            let response: AdminResponse = bytes.try_into().unwrap();
            assert_matches!(response, AdminResponse::AppInterfaceAttached{ .. });
            async { Ok(()) }.boxed()
        };
        let respond = Box::new(respond);
        let msg = WebsocketMessage::Request(msg, respond);
        handle_incoming_message(msg, admin_api).await.unwrap();
    }

    #[tokio::test(threaded_scheduler)]
    async fn dump_state() {
        observability::test_run().ok();
        let uuid = Uuid::new_v4();
        let dna = fake_dna_zomes(
            &uuid.to_string(),
            vec![("zomey".into(), TestWasm::Foo.into())],
        );
        let cell_id = CellId::from((dna.dna_hash().clone(), fake_agent_pubkey_1()));

        let mut dna_store = MockDnaStore::new();
        dna_store.expect_get().returning(move |_| Some(dna.clone()));

        let (_tmpdir, conductor_handle) =
            setup_admin_fake_cells(&[cell_id.clone()], dna_store).await;

        // Set some state
        let cell_env = conductor_handle.get_cell_env(&cell_id).await.unwrap();

        // Get state
        let expected = {
            let env = cell_env.guard().await;
            let reader = env.reader().unwrap();
            let source_chain = SourceChainBuf::new(&reader, &env).unwrap();
            source_chain.dump_as_json().await.unwrap()
        };

        let admin_api = RealAdminInterfaceApi::new(conductor_handle);
        let msg = AdminRequest::DumpState(cell_id);
        let msg = msg.try_into().unwrap();
        let respond = move |bytes: SerializedBytes| {
            let response: AdminResponse = bytes.try_into().unwrap();
            assert_matches!(response, AdminResponse::JsonState(s) if s == expected);
            async { Ok(()) }.boxed()
        };
        let respond = Box::new(respond);
        let msg = WebsocketMessage::Request(msg, respond);
        handle_incoming_message(msg, admin_api).await.unwrap();
    }
}