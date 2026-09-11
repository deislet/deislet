//! Control client: registers this instance with deis-control, keeps the
//! heartbeat going, and applies the commands and configuration it pushes.
//!
//! Commands arrive as `CommandRequest { command, args_json }` where `command`
//! is one of deis-control's command type names. Process-level commands are handed
//! to [`crate::restart::ProcessControl`]; the rest are answered here.
//!
//! Configuration arrives as `ConfigUpdate { key, value_json, version }`. The
//! keys this instance understands:
//!
//! - `apps`: the complete application list; replaces everything.
//! - `app.<id>`: one application, inserted or updated.
//! - `system`: a system configuration overlay (traffic policy only, see
//!   [`ControlSystemConfig`]).
//!
//! # Two transports, one channel
//!
//! The channel itself - `deis.control.ControlService/EstablishChannel`, the
//! stream registration, heartbeats, commands and configuration pushes all run
//! on - is the same either way. What differs is the connection under it, and
//! [`control_client`] is the one place that chooses.
//!
//! Without a `[machine_identity]` table, the connection is a plaintext dial of
//! `[control] addr`, the control plane's human listener, exactly as it has
//! always been.
//!
//! With one, the connection is made by `deis_identity::Dialler` to
//! `machine_identity.control_endpoint`: mutual TLS, this proxy proving the one
//! name its certificate carries and refusing to talk to anything that cannot
//! prove the control plane's name back. The same channel the routing-table
//! poller already runs on; see [`crate::catalog::client`], which made the same
//! move for the catalog first.
//!
//! The fleet secret is presented in the handshake only if the configuration
//! still names one. See [`machine_channel`] for why a proxy with an identity
//! no longer needs to.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use deis_identity::{Dialler, MachineIdentity, Protocol};
use deis_wire::pb::control_message::Payload;
use deis_wire::pb::{CommandRequest, CommandResponse};
use deis_wire::{ControlConfig, NodeInfo, NodeRole, SdkResult, WireClient, WireError};
use serde_json::Value as JsonValue;
use tokio::sync::broadcast;
use tokio::time::{Duration, MissedTickBehavior, interval, timeout};
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, info, warn};

use crate::config::system::MachineIdentityConfig;
use crate::config::{
    AppConfig, AppConfigEntry, ConfigError, ConfigManager, ControlAppsConfig, ControlSystemConfig,
    Validatable,
};
use crate::control::state::{ProxyStateManager, State};
use crate::restart::ProcessCommand;

/// `ConfigUpdate` key carrying the complete application list.
pub const CONFIG_KEY_APPS: &str = "apps";
/// `ConfigUpdate` key prefix for a single application (`app.<id>`).
pub const CONFIG_KEY_APP_PREFIX: &str = "app.";
/// `ConfigUpdate` key carrying a system configuration overlay.
pub const CONFIG_KEY_SYSTEM: &str = "system";

/// What a `restart` command is answered with. It states the queued state
/// rather than an outcome, because the outcome is not observed on this
/// channel: the replacement process announces itself, or this one logs the
/// failure and keeps serving.
pub const UPGRADE_QUEUED: &str = "upgrade queued, not yet done: a replacement process is being started and this process cannot observe whether it comes up. Completion is the replacement registering - a new session for this associated_id at GET /api/sessions. If the replacement fails to start, this process logs 'process command failed' and goes on serving unchanged";

/// Delay policy between connection attempts: doubling from `first` up to
/// `max`, starting over after every successful registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

/// What a failed connection attempt tells the retry loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The control plane still hears the previous holder of this node id;
    /// resolves on its own within a heartbeat timeout.
    HeldByLiveChannel,
    /// A wrong secret or a registration the control plane will not take;
    /// retrying alone will not change it.
    Rejected,
    /// Anything else - unreachable, timed out, closed mid-handshake.
    Transient,
}

fn refusal_kind(error: &WireError) -> Refusal {
    match error {
        WireError::RegistrationError(text)
            if text.contains(deis_wire::conventions::NODE_ID_HELD_BY_LIVE_CHANNEL) =>
        {
            Refusal::HeldByLiveChannel
        }
        WireError::AuthenticationError(_) | WireError::RegistrationError(_) => Refusal::Rejected,
        _ => Refusal::Transient,
    }
}

/// The two flags a zero-downtime upgrade uses to hand the control channel
/// from the old process to its replacement: `requested` is raised by the
/// upgrade path before it asks pingora for the socket transfer, `released`
/// is raised by the client once it has closed its channel. Process-wide
/// because the client is owned by a pingora background service the upgrade
/// path has no handle to; a `watch` rather than a `Notify` so a request made
/// before the client started looking is not lost.
struct Handover {
    requested: tokio::sync::watch::Sender<bool>,
    released: tokio::sync::watch::Sender<bool>,
}

fn handover() -> &'static Handover {
    static HANDOVER: std::sync::OnceLock<Handover> = std::sync::OnceLock::new();
    HANDOVER.get_or_init(|| Handover {
        requested: tokio::sync::watch::channel(false).0,
        released: tokio::sync::watch::channel(false).0,
    })
}

/// Whether a control client exists in this process at all. Set by
/// [`ProxyControlClient::new`]; an instance with no `[control]` section has
/// nothing to release and must not wait for it.
static CLIENT_PRESENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ask the control client to close its channel and wait up to `patience`
/// for it to say so. Returns whether it did; the caller proceeds either way,
/// because the hand-over is more important than the channel, but says in its
/// log which of the two happened.
pub async fn release_control_channel_for_handover(patience: Duration) -> bool {
    if !CLIENT_PRESENT.load(std::sync::atomic::Ordering::Acquire) {
        return true;
    }
    let flags = handover();
    let mut released = flags.released.subscribe();
    flags.requested.send_replace(true);
    if *released.borrow_and_update() {
        return true;
    }
    timeout(patience, async {
        while released.changed().await.is_ok() {
            if *released.borrow_and_update() {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

/// The mutual-TLS connection the control channel runs on when this proxy has
/// an identity.
struct MachineChannel {
    /// What the channel is built from, rather than the channel itself.
    ///
    /// A `tonic` channel spawns a task of its own the moment it is built - the
    /// worker behind the buffer every call goes through - so it belongs to the
    /// runtime that built it. `main` builds this client on a throwaway
    /// initialization runtime that is shut down before pingora takes over, so
    /// a channel built there would lose that worker before the first
    /// connection was ever made, and every call on it afterwards would fail
    /// for a reason with nothing to do with the control plane. The two halves
    /// below cost nothing to hold and are bound to no runtime; the channel is
    /// made from them at the first connection, on the runtime that will serve
    /// it. See [`ProxyControlClient::open_channel`].
    endpoint: Endpoint,
    dialler: Dialler,
    /// `machine_identity.control_endpoint` exactly as it was written, for log
    /// lines. `[control] addr` names the human listener and says nothing
    /// about where this connection goes, so a log line that printed it would
    /// name the wrong machine.
    written: String,
    /// Built at the first connection and kept for the life of the process.
    ///
    /// One channel, not one per attempt. A `tonic` channel reconnects on its
    /// own - every break is followed by a redial through the connector it was
    /// built with - and the connector here is a [`Dialler`], which resolves
    /// the certificate material from the identity at the moment it dials
    /// rather than capturing it when it was made. So a renewal that
    /// `MachineIdentityWatch` picks up off disk is presented by the next
    /// redial, with nothing here told that anything changed, and nothing here
    /// left presenting a certificate that has quietly expired.
    channel: Option<Channel>,
}

/// Work out what the control channel will be built from when
/// `[machine_identity]` is configured: mutual TLS to
/// `machine_identity.control_endpoint`, proving `identity`, requiring the
/// control plane's own name to be proved back.
///
/// Everything that can be refused is refused here, at startup, where an
/// operator is watching: an endpoint that is not a usable URI, and an identity
/// that names no control plane to expect. What is left for the first
/// connection is only the part that needs a runtime.
///
/// # What happened to the secret on it
///
/// Nothing has to be put in the handshake's `secret` field on this channel any
/// more. Authentication on `EstablishChannel` is the control plane's decision,
/// and it now makes it by the name the connection proved: a proxy that
/// completed this handshake has presented a certificate carrying exactly one
/// name in the fleet's trust domain, which is more than any shared value could
/// ever say about who is calling. So `[control] secret` can be left out of a
/// proxy's file entirely, and the field goes out empty.
///
/// It is still sent when it is configured, because a fleet moves one process
/// at a time and a control plane that has not yet been updated is still
/// comparing it. That is the only reason left; a proxy with an identity and no
/// secret is the finished state, not a degraded one.
fn machine_channel(
    control: &ControlConfig,
    identity: &Arc<MachineIdentity>,
    config: &MachineIdentityConfig,
) -> SdkResult<MachineChannel> {
    let expected = config.expected_control_identity().map_err(|error| {
        WireError::ConfigurationError(format!(
            "machine_identity does not name a control plane to expect: {error}. A connection that \
             required no name would accept any certificate this trust root signed, which is the \
             failure the machine channel exists to close"
        ))
    })?;
    let endpoint = Endpoint::from_shared(config.control_endpoint.clone())
        .map_err(|error| {
            WireError::ConfigurationError(format!(
                "machine_identity.control_endpoint {:?} is not a usable URI: {error}",
                config.control_endpoint
            ))
        })?
        // The connect only. Deliberately not `Endpoint::timeout`, which is a
        // deadline on every call made on the channel: `EstablishChannel` is a
        // stream that is meant to stay open for the life of the process, and a
        // per-call deadline would tear it down on a schedule. What bounds a
        // connection attempt as a whole is `connect_with_backoff` below.
        .connect_timeout(control.request_timeout_duration());
    Ok(MachineChannel {
        endpoint,
        dialler: Dialler::new(
            Arc::clone(identity),
            expected,
            // HTTP/2 because that is what gRPC is, asked for by ALPN rather
            // than hoped for, so a control plane that would only speak
            // HTTP/1.1 fails at the handshake where the reason is legible.
            &[Protocol::Http2],
        ),
        written: config.control_endpoint.clone(),
        channel: None,
    })
}

/// Build the control client this configuration asks for: the machine channel
/// if this proxy has an identity, a plaintext dial of `[control] addr` if it
/// has not.
///
/// The choice lives here rather than at the call site in `main` so that it is
/// one function with one rule in it, and so a test can ask what a given
/// configuration produces without starting a proxy. It is the same shape, for
/// the same reasons, as [`crate::catalog::routing_table_client`].
pub fn control_client(
    control: &ControlConfig,
    identity: Option<(&Arc<MachineIdentity>, &MachineIdentityConfig)>,
) -> SdkResult<ProxyControlClient> {
    let client = ProxyControlClient::new(control.clone())?;
    match identity {
        Some((identity, config)) => {
            Ok(client.over_machine_channel(machine_channel(control, identity, config)?))
        }
        None => Ok(client),
    }
}

pub struct ProxyControlClient {
    deis_wire_client: WireClient,
    state_manager: Arc<ProxyStateManager>,
    config: ControlConfig,
    backoff: Backoff,
    /// The ports announced at registration. Kept rather than only handed to
    /// the wire client, because the wire client is rebuilt whenever another
    /// builder changes what it runs on, and a rebuild must not quietly go
    /// back to announcing the defaults.
    ports: Vec<u16>,
    /// The connection the channel runs on, when this proxy has an identity.
    /// `None` leaves the wire client dialling `[control] addr` itself, which
    /// is what a proxy without one has always done.
    channel: Option<MachineChannel>,
    /// Where commands that act on the whole process are sent.
    process_tx: Option<broadcast::Sender<ProcessCommand>>,
    /// Configuration store updated by pushes. Absent in tests.
    config_manager: Option<Arc<ConfigManager>>,
    started_at: Instant,
}

enum Event {
    Heartbeat,
    Received(Result<Option<Payload>, WireError>),
}

impl ProxyControlClient {
    /// Create a client announcing the default HTTP/HTTPS ports. Use
    /// [`with_listen_ports`](Self::with_listen_ports) to announce the real ones.
    pub fn new(config: ControlConfig) -> SdkResult<Self> {
        config.validate().map_err(WireError::ConfigurationError)?;
        CLIENT_PRESENT.store(true, std::sync::atomic::Ordering::Release);
        // Accepted, like the control plane accepts it, and flagged: the
        // node endpoints it opens have no lockout.
        if let Some(advisory) = config.secret_advisory() {
            warn!(instance = %config.associated_id, "{advisory}");
        }
        let state_manager = Arc::new(ProxyStateManager::new(config.associated_id.clone()));
        let ports = vec![80, 443];
        let deis_wire_client = Self::build_deis_wire_client(&config, ports.clone(), None);
        Ok(Self {
            deis_wire_client,
            state_manager,
            config,
            backoff: Backoff::default(),
            ports,
            channel: None,
            process_tx: None,
            config_manager: None,
            started_at: Instant::now(),
        })
    }

    fn build_deis_wire_client(
        config: &ControlConfig,
        ports: Vec<u16>,
        channel: Option<Channel>,
    ) -> WireClient {
        let node_info = NodeInfo::new(
            NodeRole::Proxy(env!("CARGO_PKG_VERSION").to_string()),
            "0.0.0.0".to_string(),
            ports,
        );
        match channel {
            Some(channel) => WireClient::on_channel(config.clone(), node_info, channel),
            None => WireClient::new(config.clone(), node_info),
        }
    }

    /// Rebuild the wire client from what this one is now configured with.
    ///
    /// Everything that changes an input goes through here, so the order it
    /// happens in does not matter. That is not theoretical: the channel is
    /// made at the first connection, long after `main` has announced the
    /// listening ports, and a rebuild that read the ports from anywhere but
    /// this client would quietly go back to announcing the defaults at the
    /// moment the proxy moved onto the machine channel. Nothing live is
    /// discarded either - the builders run before [`Self::run`], and
    /// [`Self::open_channel`] rebuilds only on the attempt that has no
    /// connection yet.
    fn rebuild_deis_wire_client(&mut self) {
        self.deis_wire_client = Self::build_deis_wire_client(
            &self.config,
            self.ports.clone(),
            // Cloning a tonic channel shares one connection and its reconnect
            // logic rather than opening a second.
            self.channel
                .as_ref()
                .and_then(|machine| machine.channel.clone()),
        );
    }

    /// Announce the ports this instance actually listens on.
    pub fn with_listen_ports(mut self, ports: Vec<u16>) -> Self {
        self.ports = ports;
        self.rebuild_deis_wire_client();
        self
    }

    /// Run the control channel on the machine channel, which is what a
    /// `[machine_identity]` table asks for. See [`machine_channel`] for what
    /// is settled here and what is left until the first connection, and
    /// [`control_client`] for the choice itself.
    ///
    /// The wire client is not rebuilt here: there is no channel yet to give
    /// it, only what one will be made from.
    fn over_machine_channel(mut self, channel: MachineChannel) -> Self {
        self.channel = Some(channel);
        self
    }

    /// Which connection this client's channel runs on, in the words a log
    /// line has room for. Also what a test asserts on to check the choice was
    /// made the way the configuration asked for.
    pub fn transport(&self) -> &'static str {
        match &self.channel {
            Some(_) => "machine channel",
            None => "human listener",
        }
    }

    /// Where this client actually connects, as the operator wrote it - which
    /// is `machine_identity.control_endpoint` and not `[control] addr` once
    /// the machine channel is in use.
    pub fn dialled(&self) -> &str {
        match &self.channel {
            Some(machine) => &machine.written,
            None => &self.config.addr,
        }
    }

    /// Attach the configuration store that pushes are applied to.
    pub fn with_config_manager(mut self, config_manager: Arc<ConfigManager>) -> Self {
        self.config_manager = Some(config_manager);
        self
    }

    /// Attach the channel that process-level commands are dispatched on.
    pub fn set_process_channel(&mut self, process_tx: broadcast::Sender<ProcessCommand>) {
        self.process_tx = Some(process_tx);
    }

    /// Override the delay policy between connection attempts.
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Serve until cancelled: connect (retrying until the control
    /// answers), handle heartbeats and messages, and reconnect whenever the
    /// connection is lost. Never returns on its own; the caller races it
    /// against the shutdown signal.
    pub async fn run(&mut self) {
        let mut handover = handover().requested.subscribe();
        loop {
            // A hand-over asked for before this loop looked is still a
            // hand-over: `borrow_and_update` reads the value whether or not
            // `changed` would have fired for it.
            if *handover.borrow_and_update() {
                return self.stand_down_for_handover().await;
            }
            tokio::select! {
                biased;
                _ = handover.changed() => return self.stand_down_for_handover().await,
                _ = self.connect_with_backoff() => {}
            }
            let served = tokio::select! {
                biased;
                _ = handover.changed() => None,
                result = self.serve() => Some(result),
            };
            match served {
                None => return self.stand_down_for_handover().await,
                Some(Err(e)) => warn!(
                    instance = %self.config.associated_id,
                    error = %e,
                    "control connection lost; reconnecting"
                ),
                Some(Ok(())) => {}
            }
        }
    }

    /// Give the node id back to the control plane so the replacement process
    /// can take it, then stay off the channel for the rest of this process's
    /// life.
    ///
    /// Without this a zero-downtime upgrade was an error storm: the old
    /// process kept its channel alive through the grace period, the control
    /// plane rightly refused the replacement's registration for an id whose
    /// holder it still heard from, and the replacement retried at error level
    /// with doubling delay until the old channel finally went silent. Routes
    /// come from the catalog so traffic never noticed, but commands and
    /// configuration pushes were delayed by a heartbeat timeout plus the
    /// backoff, and the log said something was wrong when nothing was.
    ///
    /// Never returns: `run`'s caller treats a return as the client dying,
    /// and this process is about to exit anyway.
    async fn stand_down_for_handover(&mut self) {
        info!(
            instance = %self.config.associated_id,
            "releasing the control channel so the replacement process can register"
        );
        if let Err(e) = self.disconnect().await {
            warn!(
                instance = %self.config.associated_id,
                error = %e,
                "the control channel did not close cleanly; the control plane gives it up after \
                 its heartbeat timeout"
            );
        }
        handover().released.send_replace(true);
        std::future::pending::<()>().await
    }

    /// Connect, authenticate and register, retrying with capped exponential
    /// backoff until it works. Each attempt is bounded by the configured
    /// request timeout so a control that accepts TCP but never answers
    /// cannot park the loop. Failed attempts are logged at WARN until
    /// `retry_count` of them have failed, then at ERROR. A rejected secret or
    /// registration is ERROR from the first attempt because retrying alone
    /// will not fix it; the loop still keeps going so a fix on the control
    /// side takes effect without restarting deis-proxy.
    pub async fn connect_with_backoff(&mut self) {
        let mut delay = self.backoff.first;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            info!(
                instance = %self.config.associated_id,
                addr = %self.dialled(),
                transport = self.transport(),
                attempt,
                "connecting to control"
            );
            let limit = self.config.request_timeout_duration();
            let result = match timeout(limit, self.connect_and_register()).await {
                Ok(result) => result,
                Err(_) => Err(WireError::ConnectionError(format!(
                    "no answer from the control within {}s",
                    limit.as_secs()
                ))),
            };
            let error = match result {
                Ok(()) => {
                    info!(
                        instance = %self.config.associated_id,
                        attempt,
                        "registered with control"
                    );
                    return;
                }
                Err(error) => error,
            };

            // Drop any half-open channel before trying again.
            let _ = self.deis_wire_client.disconnect().await;
            let refusal = refusal_kind(&error);
            if refusal == Refusal::HeldByLiveChannel {
                // The common reading is the benign one: this process is the
                // replacement in an upgrade, or is reconnecting after a break
                // the control plane has not noticed. It resolves within a
                // heartbeat timeout on its own, so the retry stays short and
                // the log stays at warn. Two processes sharing an id look the
                // same from here and keep warning until someone reads it.
                warn!(
                    instance = %self.config.associated_id,
                    attempt,
                    error = %error,
                    next_attempt_in = ?self.backoff.first,
                    "the control plane still hears the previous holder of this node id; retrying \
                     until it gives that channel up"
                );
                tokio::time::sleep(self.backoff.first).await;
                continue;
            }
            let rejected = refusal == Refusal::Rejected;
            if rejected || attempt >= self.config.retry_count {
                error!(
                    instance = %self.config.associated_id,
                    transport = self.transport(),
                    attempt,
                    error = %error,
                    next_attempt_in = ?delay,
                    "control connection attempt failed"
                );
            } else {
                warn!(
                    instance = %self.config.associated_id,
                    transport = self.transport(),
                    attempt,
                    error = %error,
                    next_attempt_in = ?delay,
                    "control connection attempt failed"
                );
            }
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2).min(self.backoff.max);
        }
    }

    /// Open the channel, building it first if this is the first connection.
    ///
    /// The one place `WireClient::connect` is called from, and it must stay
    /// that way: a call that skipped this would find a wire client that had no
    /// channel yet and would quietly dial `[control] addr` in plaintext -
    /// which is to say a proxy configured for mutual TLS would fall back to
    /// the fleet secret without anything saying so.
    ///
    /// Building here rather than at construction is what keeps the channel on
    /// the runtime that will serve it; see [`MachineChannel`].
    async fn open_channel(&mut self) -> SdkResult<()> {
        if let Some(machine) = &mut self.channel
            && machine.channel.is_none()
        {
            machine.channel = Some(
                machine
                    .endpoint
                    .connect_with_connector_lazy(machine.dialler.clone()),
            );
            self.rebuild_deis_wire_client();
        }
        self.deis_wire_client.connect().await
    }

    async fn connect_and_register(&mut self) -> SdkResult<()> {
        self.open_channel().await?;
        self.deis_wire_client.authenticate().await?;
        self.deis_wire_client.register().await?;
        self.state_manager
            .handle_registration_complete()
            .await
            .map_err(|e| WireError::ConfigurationError(format!("state machine error: {e}")))?;
        Ok(())
    }

    /// Heartbeats and incoming messages on the current connection; returns
    /// once it fails.
    async fn serve(&mut self) -> SdkResult<()> {
        let mut heartbeat = interval(self.config.heartbeat_duration());
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            let event = tokio::select! {
                _ = heartbeat.tick() => Event::Heartbeat,
                received = self.deis_wire_client.receive_message() => Event::Received(received),
            };

            match event {
                Event::Heartbeat => {
                    self.deis_wire_client.send_heartbeat().await?;
                    debug!(instance = %self.config.associated_id, "heartbeat sent");
                }
                Event::Received(Ok(Some(payload))) => self.handle_payload(payload).await,
                Event::Received(Ok(None)) => debug!("empty payload from control ignored"),
                Event::Received(Err(e)) => return Err(e),
            }
        }
    }

    async fn handle_payload(&mut self, payload: Payload) {
        match payload {
            Payload::CommandRequest(request) => {
                let response = self.handle_command_request(&request).await;
                if let Err(e) = self
                    .deis_wire_client
                    .send_message(Payload::CommandResponse(response))
                    .await
                {
                    warn!(command = %request.command, id = %request.id, error = %e, "command response not delivered");
                }
            }
            Payload::ConfigUpdate(update) => {
                self.apply_config_update(&update.key, &update.value_json, &update.version)
                    .await
            }
            Payload::AppConfigs(apps) => {
                self.apply_config_update(CONFIG_KEY_APPS, &apps.configs_json, &apps.version)
                    .await
            }
            Payload::SystemConfig(system) => {
                self.apply_config_update(CONFIG_KEY_SYSTEM, &system.config_json, &system.version)
                    .await
            }
            Payload::AuthResponse(_) | Payload::RegisterResponse(_) => {
                debug!("late handshake response ignored")
            }
            other => debug!(?other, "unexpected payload from control ignored"),
        }
    }

    /// Execute a command and build the response to send back.
    pub async fn handle_command_request(&self, request: &CommandRequest) -> CommandResponse {
        info!(command = %request.command, id = %request.id, "command received");
        let outcome: Result<String, String> = match request.command.as_str() {
            "shutdown" => self
                .dispatch(ProcessCommand::Shutdown)
                .await
                .map(|_| "graceful shutdown initiated".to_string()),
            // What is answered here is that the upgrade was *queued*: the
            // process-control loop starts the replacement after this reply
            // has gone, and this process cannot see whether that works. It
            // used to say "initiated", which the control plane recorded as
            // Completed while the replacement had failed to bind its socket
            // and nothing had changed. The truthful completion signal is the
            // replacement registering: a new session for this associated_id
            // at GET /api/sessions.
            "restart" | "reload" | "graceful_restart" => self
                .dispatch(ProcessCommand::Upgrade)
                .await
                .map(|_| UPGRADE_QUEUED.to_string()),
            "get_status" => Ok(self.status().await.to_string()),
            "get_metrics" => Err("metrics are not implemented".to_string()),
            other => Err(format!("unknown command: {other}")),
        };

        match outcome {
            Ok(message) => CommandResponse {
                id: request.id.clone(),
                success: true,
                message,
                error: String::new(),
            },
            Err(error) => {
                warn!(command = %request.command, id = %request.id, %error, "command rejected");
                CommandResponse {
                    id: request.id.clone(),
                    success: false,
                    message: String::new(),
                    error,
                }
            }
        }
    }

    async fn dispatch(&self, command: ProcessCommand) -> Result<(), String> {
        let Some(process_tx) = &self.process_tx else {
            return Err("process control is not attached".to_string());
        };
        if command == ProcessCommand::Shutdown {
            self.state_manager.handle_shutdown().await?;
        }
        process_tx
            .send(command)
            .map(|_| ())
            .map_err(|e| format!("process control is not listening: {e}"))
    }

    /// Status report answered to `get_status`.
    pub async fn status(&self) -> JsonValue {
        serde_json::json!({
            "instance_id": self.config.associated_id,
            "version": env!("CARGO_PKG_VERSION"),
            "state": format!("{:?}", self.state_manager.current_state().await),
            "is_active": self.state_manager.is_active().await,
            "uptime_secs": self.started_at.elapsed().as_secs(),
            "transition_count": self.state_manager.history_count().await,
        })
    }

    async fn apply_config_update(&self, key: &str, value_json: &str, version: &str) {
        let Some(config_manager) = &self.config_manager else {
            warn!(
                key,
                "configuration push ignored: no configuration store attached"
            );
            return;
        };
        let version = DateTime::parse_from_rfc3339(version)
            .map(|v| v.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        let outcome = if key == CONFIG_KEY_APPS {
            match ControlAppsConfig::from_json(value_json) {
                Ok(apps) => {
                    config_manager
                        .replace_app_configs(apps.get_apps(), version)
                        .await
                }
                Err(e) => Err(e),
            }
        } else if let Some(app_id) = key.strip_prefix(CONFIG_KEY_APP_PREFIX) {
            match serde_json::from_str::<AppConfig>(value_json)
                .map_err(ConfigError::from)
                .and_then(|app| app.validate().map(|_| app))
            {
                Ok(app) if app.id != app_id => Err(ConfigError::ControlError(format!(
                    "app id {} in the payload does not match key {key}",
                    app.id
                ))),
                Ok(app) => match serde_json::to_value(&app) {
                    Ok(config) => config_manager
                        .update_app_configs(vec![AppConfigEntry {
                            app_id: app_id.to_string(),
                            config,
                            version_timestamp: version,
                        }])
                        .await
                        .map(|_| ()),
                    Err(e) => Err(ConfigError::from(e)),
                },
                Err(e) => Err(e),
            }
        } else if key == CONFIG_KEY_SYSTEM {
            ControlSystemConfig::from_json(value_json)
                .and_then(|overlay| config_manager.apply_system_overlay(overlay))
        } else {
            Err(ConfigError::ControlError(format!(
                "unknown configuration key: {key}"
            )))
        };

        match outcome {
            Ok(()) => info!(key, %version, "configuration applied"),
            Err(e) => error!(key, %version, error = %e, "configuration push rejected"),
        }
    }

    pub async fn is_ready(&self) -> bool {
        self.deis_wire_client.is_connected()
            && self.deis_wire_client.is_authenticated()
            && self.deis_wire_client.is_registered()
            && self.state_manager.is_active().await
    }

    /// Tear the connection down on shutdown. The lifecycle transitions only
    /// apply to an instance that actually registered; one that is shutting
    /// down while still waiting for the control has nothing to unwind.
    pub async fn disconnect(&mut self) -> SdkResult<()> {
        if self.state_manager.is_active().await {
            let _ = self.state_manager.handle_shutdown().await;
        }
        if self.state_manager.is_sunsetting().await {
            let _ = self.state_manager.handle_silenced().await;
        }
        if self.deis_wire_client.is_connected() {
            self.deis_wire_client.disconnect().await?;
            info!(instance = %self.config.associated_id, "disconnected from control");
        } else {
            info!(
                instance = %self.config.associated_id,
                "shutting down before the control connection was established"
            );
        }
        Ok(())
    }

    pub async fn current_state(&self) -> State {
        self.state_manager.current_state().await
    }

    #[cfg(test)]
    pub fn state_manager(&self) -> &Arc<ProxyStateManager> {
        &self.state_manager
    }
}

#[cfg(test)]
mod tests {
    use deis_identity::names;
    use deis_identity_test_pki::{TestCa, write_files};

    use super::*;
    use crate::control::mock::MockControl;

    #[test]
    fn a_refusal_for_a_node_id_still_held_is_transient_and_the_others_are_not() {
        let held = WireError::RegistrationError(format!(
            "node id edge-1 {} (instance abc, last heard 2s ago, heartbeat_timeout 90s)",
            deis_wire::conventions::NODE_ID_HELD_BY_LIVE_CHANNEL
        ));
        assert_eq!(refusal_kind(&held), Refusal::HeldByLiveChannel);

        let unknown = WireError::RegistrationError("node id edge-1 is not in the fleet".into());
        assert_eq!(refusal_kind(&unknown), Refusal::Rejected);
        let wrong_secret = WireError::AuthenticationError("secret refused".into());
        assert_eq!(refusal_kind(&wrong_secret), Refusal::Rejected);
        let unreachable = WireError::ConnectionError("no answer within 5s".into());
        assert_eq!(refusal_kind(&unreachable), Refusal::Transient);
    }

    #[tokio::test]
    async fn a_handover_request_made_before_the_client_looked_is_still_seen() {
        // `run` subscribes after the process has been alive for a while; a
        // request raised before that subscription must not be lost, which
        // is what `borrow_and_update` on a fresh receiver guarantees and a
        // `Notify` would not.
        let (requested, _keep) = tokio::sync::watch::channel(false);
        requested.send_replace(true);
        let mut late = requested.subscribe();
        assert!(
            *late.borrow_and_update(),
            "a late subscriber reads the raised flag"
        );
    }

    #[tokio::test]
    async fn an_instance_with_no_control_client_has_nothing_to_release() {
        // Only meaningful when no client was constructed in this test
        // process: `CLIENT_PRESENT` is process-wide, so this asserts the
        // cheap path on whichever side of it the process is on.
        if !CLIENT_PRESENT.load(std::sync::atomic::Ordering::Acquire) {
            assert!(release_control_channel_for_handover(Duration::from_millis(10)).await);
        }
    }

    /// A loaded identity naming one machine in `deis.internal`, signed by
    /// `ca`. `serves` is what that machine's own listener will advertise -
    /// empty for the proxy, which only dials.
    fn identity(
        ca: &TestCa,
        directory: &std::path::Path,
        name: &str,
        id: deis_identity::SpiffeId,
        serves: &[Protocol],
    ) -> (Arc<MachineIdentity>, MachineIdentityFiles) {
        let files = write_files(directory, name, &ca.issue(&id), &ca.root_pem);
        let identity = Arc::new(
            MachineIdentity::load(files.as_identity_files(), serves).expect("an identity"),
        );
        let paths = MachineIdentityFiles {
            certificate: files.certificate.display().to_string(),
            private_key: files.private_key.display().to_string(),
            trust_root: files.trust_root.display().to_string(),
        };
        (identity, paths)
    }

    /// The three paths as they are written in a configuration file.
    struct MachineIdentityFiles {
        certificate: String,
        private_key: String,
        trust_root: String,
    }

    fn machine_identity_config(
        files: &MachineIdentityFiles,
        control_endpoint: &str,
    ) -> MachineIdentityConfig {
        MachineIdentityConfig {
            certificate: files.certificate.clone(),
            private_key: files.private_key.clone(),
            trust_root: files.trust_root.clone(),
            identity_reload_seconds: 60,
            trust_domain: TRUST_DOMAIN.to_string(),
            control_endpoint: control_endpoint.to_string(),
            ..Default::default()
        }
    }

    const TRUST_DOMAIN: &str = "deis.internal";

    #[test]
    fn no_identity_leaves_the_control_channel_exactly_where_it_was() {
        let client = control_client(&test_config(), None).unwrap();
        assert_eq!(client.transport(), "human listener");
        assert_eq!(
            client.dialled(),
            "http://localhost:8080",
            "a proxy with no certificate still dials [control] addr"
        );
    }

    /// Deliberately not a `#[tokio::test]`: this is how `main` builds the
    /// client - on the initialization path, outside the throwaway runtime it
    /// uses there and long before pingora's own runtime exists. A client that
    /// made its channel at construction would spawn that channel's worker
    /// onto a runtime that is shut down before the first connection, or, with
    /// none running at all, panic here.
    #[test]
    fn an_identity_moves_the_control_channel_onto_the_machine_channel() {
        let directory = tempfile::tempdir().expect("a directory");
        let ca = TestCa::new();
        let (identity, files) = identity(
            &ca,
            directory.path(),
            "proxy",
            names::proxy(TRUST_DOMAIN, "test-worker").expect("a proxy name"),
            &[],
        );
        let config = machine_identity_config(&files, "https://control.internal:8443");
        config.validate().expect("a complete table");

        let client = control_client(&test_config(), Some((&identity, &config))).unwrap();
        assert_eq!(client.transport(), "machine channel");
        assert_eq!(
            client.dialled(),
            "https://control.internal:8443",
            "the machine channel is a listener of its own on a port of its own, not [control] addr"
        );

        // Order-independence: the builders that follow in `main` must not put
        // the transport back, and the transport must not put the announced
        // ports back to the defaults.
        let client = client.with_listen_ports(vec![8080, 8443]);
        assert_eq!(client.transport(), "machine channel");
        assert_eq!(client.ports, vec![8080, 8443]);
    }

    #[test]
    fn an_identity_whose_control_name_cannot_be_built_is_reported_rather_than_dialled() {
        // Unreachable through a validated configuration - `[machine_identity]`
        // refuses this at startup - but the two checks are in different files,
        // and a client that quietly dialled a nameless peer would be trusting
        // whatever certificate answered.
        let directory = tempfile::tempdir().expect("a directory");
        let ca = TestCa::new();
        let (identity, files) = identity(
            &ca,
            directory.path(),
            "proxy",
            names::proxy(TRUST_DOMAIN, "test-worker").expect("a proxy name"),
            &[],
        );
        let config = MachineIdentityConfig {
            trust_domain: String::new(),
            ..machine_identity_config(&files, "https://control.internal:8443")
        };
        // Matched rather than unwrapped: `ProxyControlClient` holds a channel
        // and a state machine, neither of which has a `Debug`, so the success
        // case cannot be printed by `expect_err`.
        let error = match control_client(&test_config(), Some((&identity, &config))) {
            Ok(_) => panic!("a peer with no name is not a peer"),
            Err(error) => error,
        };
        assert!(
            matches!(error, WireError::ConfigurationError(_)),
            "{error:?}"
        );
    }

    /// One fleet: a control plane that serves the machine channel, and a proxy
    /// that dials it.
    struct Fleet {
        control: Arc<MachineIdentity>,
        proxy: Arc<MachineIdentity>,
        files: MachineIdentityFiles,
        _directory: tempfile::TempDir,
    }

    fn fleet() -> Fleet {
        let directory = tempfile::tempdir().expect("a directory");
        let ca = TestCa::new();
        let (control, _) = identity(
            &ca,
            directory.path(),
            "control",
            names::control(TRUST_DOMAIN).expect("a control name"),
            // The machine listener carries gRPC, and saying so at the
            // handshake is what the proxy's dialler asks for.
            &[Protocol::Http2],
        );
        let (proxy, files) = identity(
            &ca,
            directory.path(),
            "proxy",
            names::proxy(TRUST_DOMAIN, "test-worker").expect("a proxy name"),
            &[],
        );
        Fleet {
            control,
            proxy,
            files,
            _directory: directory,
        }
    }

    /// The whole path: a listener that will not talk to an unnamed caller, a
    /// proxy that proves a name to reach it, and a control channel that
    /// registers over the connection they agreed on.
    #[tokio::test]
    async fn a_proxy_with_an_identity_registers_over_mutual_tls() {
        let fleet = fleet();
        let mut control = MockControl::start_over_mutual_tls(Arc::clone(&fleet.control)).await;
        let config = machine_identity_config(&fleet.files, &control.addr);
        config.validate().expect("a complete table");

        let mut client = control_client(&test_config(), Some((&fleet.proxy, &config)))
            .unwrap()
            .with_backoff(fast_backoff());
        assert_eq!(client.transport(), "machine channel");
        // Bounded rather than left to `connect_with_backoff`, which retries
        // for ever on purpose: a handshake that cannot complete has to end
        // this test with the reason it failed, not by running out of time.
        timeout(Duration::from_secs(10), client.connect_and_register())
            .await
            .expect("a handshake, a registration, or an error - not a hang")
            .expect("the control plane proved its name and took the registration");

        assert_eq!(control.next_registration().await, 1);
        assert!(client.is_ready().await);
        assert_eq!(client.current_state().await, State::Active);
        assert_eq!(
            control.secrets(),
            vec![test_config().secret.unwrap()],
            "a secret that is configured is still put in the handshake; what changed is that it \
             is no longer what admits the connection"
        );
    }

    /// The point of the whole exercise: a proxy that proves a certificate
    /// needs no fleet secret at all, so its configuration can stop naming
    /// one, and it still registers.
    ///
    /// What the control plane receives in the handshake's `secret` field is
    /// then the empty string - the honest encoding of "nothing presented" -
    /// and it is admitted anyway, by the name it proved during the TLS
    /// handshake. A control plane that still compared that field would refuse
    /// this, which is why the mock admits a mutually authenticated connection
    /// the way the real one does.
    #[tokio::test]
    async fn a_proxy_with_an_identity_registers_with_no_fleet_secret_at_all() {
        let fleet = fleet();
        let mut control = MockControl::start_over_mutual_tls(Arc::clone(&fleet.control)).await;
        let config = machine_identity_config(&fleet.files, &control.addr);

        let mut without = test_config();
        without.secret = None;

        let mut client = control_client(&without, Some((&fleet.proxy, &config)))
            .unwrap()
            .with_backoff(fast_backoff());
        timeout(Duration::from_secs(10), client.connect_and_register())
            .await
            .expect("a handshake, a registration, or an error - not a hang")
            .expect("a proved name is enough; nothing shared should be needed");

        assert_eq!(control.next_registration().await, 1);
        assert_eq!(client.current_state().await, State::Active);
        assert_eq!(
            control.secrets(),
            vec![String::new()],
            "nothing was presented, and nothing needed to be"
        );
    }

    /// The same client against a control plane that is not speaking TLS at
    /// all: it must fail rather than quietly go on in plaintext, because a
    /// fallback would mean this proxy's certificate proves nothing about the
    /// connection its control channel actually runs on.
    #[tokio::test]
    async fn a_machine_channel_client_does_not_fall_back_to_plaintext() {
        let fleet = fleet();
        // The human listener's address, written where the machine channel's
        // belongs - the mistake `check_control_endpoint` catches when the
        // scheme gives it away, and cannot when the port is simply wrong.
        let plaintext = MockControl::start().await;
        let endpoint = plaintext.addr.replace("http://", "https://");
        let config = machine_identity_config(&fleet.files, &endpoint);

        let mut client = control_client(&test_config(), Some((&fleet.proxy, &config)))
            .unwrap()
            .with_backoff(fast_backoff());
        let error = timeout(Duration::from_secs(10), client.connect_and_register())
            .await
            .expect("a refusal, not a hang")
            .expect_err("a plaintext listener cannot prove the control plane's name");
        assert!(matches!(error, WireError::ConnectionError(_)), "{error:?}");
        assert!(
            plaintext.secrets().is_empty(),
            "nothing may be sent to a peer that never proved who it is"
        );
    }

    fn test_config() -> ControlConfig {
        config_for("http://localhost:8080")
    }

    fn config_for(addr: &str) -> ControlConfig {
        ControlConfig {
            associated_id: "test-worker".to_string(),
            secret: Some(
                "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
            ),
            addr: addr.to_string(),
            heartbeat_interval: 30,
            request_timeout: 10,
            retry_count: 3,
        }
    }

    fn fast_backoff() -> Backoff {
        Backoff {
            first: Duration::from_millis(5),
            max: Duration::from_millis(20),
        }
    }

    #[tokio::test]
    async fn registers_once_the_control_answers() {
        let mut control = MockControl::start().await;
        control.refuse_next(1);
        let mut client = ProxyControlClient::new(config_for(&control.addr))
            .unwrap()
            .with_backoff(fast_backoff());

        client.connect_with_backoff().await;

        assert_eq!(client.current_state().await, State::Active);
        assert!(client.is_ready().await);
        assert_eq!(control.next_registration().await, 2);
        assert_eq!(control.connections(), 2);
    }

    #[tokio::test]
    async fn reconnects_after_an_outage_longer_than_five_attempts() {
        let mut control = MockControl::start().await;
        let mut client = ProxyControlClient::new(config_for(&control.addr))
            .unwrap()
            .with_backoff(fast_backoff());
        client.connect_with_backoff().await;
        assert_eq!(control.next_registration().await, 1);
        let state = Arc::clone(client.state_manager());

        // Six refused attempts is more than the five the old policy allowed.
        control.refuse_next(6);
        control.cut_connections();
        let running = tokio::spawn(async move { client.run().await });

        assert_eq!(control.next_registration().await, 8);
        assert_eq!(state.current_state().await, State::Active);
        running.abort();
    }

    #[tokio::test]
    async fn disconnect_before_registration_leaves_pending() {
        let mut client = ProxyControlClient::new(test_config()).unwrap();
        client.disconnect().await.unwrap();
        assert_eq!(client.current_state().await, State::Pending);
        assert_eq!(client.state_manager().history_count().await, 0);
    }

    fn request(command: &str) -> CommandRequest {
        CommandRequest {
            id: "cmd-1".to_string(),
            command: command.to_string(),
            args_json: "null".to_string(),
        }
    }

    #[test]
    fn creates_client() {
        assert!(ProxyControlClient::new(test_config()).is_ok());
    }

    /// The proxy accepts the secret shapes the control plane accepts and
    /// refuses only a blank one; a short value starts with a warning rather
    /// than not at all (r9 tenancy-fleet F6).
    #[test]
    fn accepts_what_the_control_plane_accepts_and_refuses_only_a_blank_secret() {
        let mut config = test_config();
        config.secret = Some("r9nodekey".to_string());
        assert!(ProxyControlClient::new(config).is_ok());

        let mut config = test_config();
        config.secret = Some(" ".to_string());
        assert!(ProxyControlClient::new(config).is_err());
    }

    #[tokio::test]
    async fn starts_pending() {
        let client = ProxyControlClient::new(test_config()).unwrap();
        assert_eq!(client.current_state().await, State::Pending);
        assert_eq!(client.state_manager().instance_id(), "test-worker");
        assert!(!client.is_ready().await);
    }

    #[tokio::test]
    async fn answers_status() {
        let client = ProxyControlClient::new(test_config()).unwrap();
        let response = client.handle_command_request(&request("get_status")).await;
        assert!(response.success);
        let status: JsonValue = serde_json::from_str(&response.message).unwrap();
        assert_eq!(status["instance_id"], "test-worker");
        assert_eq!(status["state"], "Pending");
    }

    #[tokio::test]
    async fn rejects_unknown_and_unimplemented_commands() {
        let client = ProxyControlClient::new(test_config()).unwrap();
        let response = client.handle_command_request(&request("get_metrics")).await;
        assert!(!response.success);
        let response = client.handle_command_request(&request("dance")).await;
        assert!(!response.success);
        assert!(response.error.contains("unknown command"));
    }

    #[tokio::test]
    async fn process_commands_need_a_channel() {
        let mut client = ProxyControlClient::new(test_config()).unwrap();
        let response = client.handle_command_request(&request("restart")).await;
        assert!(!response.success);

        let (tx, mut rx) = broadcast::channel(2);
        client.set_process_channel(tx);
        let response = client.handle_command_request(&request("reload")).await;
        assert!(response.success, "{}", response.error);
        assert_eq!(rx.recv().await.unwrap(), ProcessCommand::Upgrade);
        // Queued is what happened; "done" is not known here and is not said.
        assert_eq!(response.message, UPGRADE_QUEUED);
        assert!(
            response.message.contains("not yet done"),
            "{}",
            response.message
        );
        assert!(
            response.message.contains("/api/sessions"),
            "{}",
            response.message
        );
    }
}
