use std::{
    collections::HashMap,
    fs,
    io::Read,
    net::SocketAddr,
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use pumpkin::{
    PumpkinServer, SHOULD_STOP, STOP_INTERRUPT, data::VanillaData, net::ClientPlatform, stop_server,
};
use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
use pumpkin_protocol::ConnectionState;
use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use tokio_util::task::TaskTracker;

const HARNESS_TIMEOUT_SECONDS: &str = "60";
const TEST_USERNAME: &str = "compat_reconfig";
const EOF_USERNAME: &str = "eof_reconfig";

struct HarnessChild {
    child: Option<Child>,
}

impl std::ops::Deref for HarnessChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        self.child.as_ref().expect("harness child is still owned")
    }
}

impl std::ops::DerefMut for HarnessChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child.as_mut().expect("harness child is still owned")
    }
}

impl Drop for HarnessChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
        }
    }
}

fn test_vanilla_data() -> VanillaData {
    VanillaData {
        banned_ip_list: std::sync::RwLock::new(Default::default()),
        banned_player_list: std::sync::RwLock::new(Default::default()),
        operator_config: std::sync::RwLock::new(Default::default()),
        user_cache: std::sync::RwLock::new(Default::default()),
        whitelist_config: std::sync::RwLock::new(Default::default()),
    }
}

fn isolated_basic_config(world_root: &Path) -> BasicConfiguration {
    let mut config = BasicConfiguration::default();
    config.default_level_name = world_root.to_string_lossy().into_owned();
    config.allow_nether = false;
    config.allow_end = false;
    config.allow_chat_reports = false;
    config.use_favicon = false;
    config
}

fn isolated_advanced_config() -> AdvancedConfiguration {
    let mut config = AdvancedConfiguration::default();
    config.logging.enabled = false;
    config.plugins.enabled = false;
    config.commands.use_console = false;
    config.commands.use_tty = false;
    config.networking.java.address = SocketAddr::from(([127, 0, 0, 1], 0));
    config.networking.java.online_mode = false;
    config.networking.java.encryption = false;
    config.networking.java.keep_alive_time = 1;
    config.networking.bedrock.enabled = false;
    config.networking.query.enabled = false;
    config.networking.lan_broadcast.enabled = false;
    config.networking.rcon.enabled = false;
    config
}

fn isolated_telemetry_config() -> TelemetryConfig {
    TelemetryConfig {
        enabled: false,
        ..TelemetryConfig::default()
    }
}

fn spawn_harness(
    port: u16,
    username: &str,
    case: Option<&str>,
    diagnostic_file: &Path,
) -> HarnessChild {
    let harness = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("crates")
        .join("pumpkin")
        .join("tests")
        .join("protocol776_reconfiguration_client.py");
    assert!(
        harness.is_file(),
        "missing protocol harness at {}",
        harness.display()
    );

    let launcher = if cfg!(windows) { "py" } else { "python3" };
    let mut command = Command::new(launcher);
    if cfg!(windows) {
        command.arg("-3");
    }
    command
        .arg(&harness)
        .arg("--mode")
        .arg("reconfiguration")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--timeout")
        .arg(HARNESS_TIMEOUT_SECONDS)
        .arg("--username")
        .arg(username)
        .arg("--known-packs-case")
        .arg("exact")
        .arg("--diagnostic-file")
        .arg(diagnostic_file)
        .arg("--reconfiguration-cycles")
        .arg(if case == Some("disconnect_during_config") {
            "1"
        } else {
            "2"
        });
    if let Some(case) = case {
        command.arg("--reconfiguration-case").arg(case);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().unwrap_or_else(|error| {
        panic!("failed to spawn required Python launcher `{launcher}`: {error}")
    });
    HarnessChild { child: Some(child) }
}

fn wait_for_harness(mut child: HarnessChild) -> Output {
    let child = child.child.take().expect("harness child is still owned");
    tokio::task::block_in_place(|| child.wait_with_output())
        .unwrap_or_else(|error| panic!("failed to collect protocol harness output: {error}"))
}

fn parse_harness_output(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "protocol harness failed: exit={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout
        .lines()
        .last()
        .unwrap_or_else(|| panic!("protocol harness produced no JSON: {stdout}"));
    serde_json::from_str(json).unwrap_or_else(|error| {
        panic!("protocol harness JSON decode failed: {error}; stdout={stdout}")
    })
}

async fn wait_for_player(
    server: &pumpkin::server::Server,
    username: &str,
    mut child: Option<&mut Child>,
) -> Arc<pumpkin::entity::player::Player> {
    timeout(Duration::from_secs(30), async {
        loop {
            if let Some(player) = server.get_player_by_name(username)
                && matches!(player.client.as_ref(), ClientPlatform::Java(_))
            {
                return player;
            }
            if let Some(child) = child.as_deref_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut stream) = child.stdout.take() {
                    let _ = stream.read_to_end(&mut stdout);
                }
                if let Some(mut stream) = child.stderr.take() {
                    let _ = stream.read_to_end(&mut stderr);
                }
                panic!(
                    "protocol harness exited before player {username}: {status}; stdout={}; stderr={}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for server-owned player {username}"))
}

async fn wait_for_state(
    java: &pumpkin::net::java::JavaClient,
    state: ConnectionState,
    mut child: Option<&mut Child>,
    diagnostic_file: Option<&Path>,
) {
    timeout(Duration::from_secs(15), async {
        loop {
            if java.connection_state.load() == state {
                return;
            }
            if let Some(child) = child.as_deref_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut stream) = child.stdout.take() {
                    let _ = stream.read_to_end(&mut stdout);
                }
                if let Some(mut stream) = child.stderr.take() {
                    let _ = stream.read_to_end(&mut stderr);
                }
                panic!(
                    "protocol harness exited before state {state:?}: {status}; stdout={}; stderr={}; diagnostic={}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr),
                    diagnostic_file.map_or_else(|| "<none>".to_string(), diagnostic_trace)
                );
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for Java connection state {state:?}"));
}

fn diagnostic_trace(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("<diagnostic unavailable: {error}>"))
}

async fn wait_for_play_baseline(
    java: &pumpkin::net::java::JavaClient,
    diagnostic_file: &Path,
    required_event: &str,
    mut child: Option<&mut HarnessChild>,
) {
    let before_play_packets = java.last_packet_time.load();
    timeout(Duration::from_secs(20), async {
        loop {
            if let Some(child) = child.as_deref_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut stream) = child.stdout.take() {
                    let _ = stream.read_to_end(&mut stdout);
                }
                if let Some(mut stream) = child.stderr.take() {
                    let _ = stream.read_to_end(&mut stderr);
                }
                panic!(
                    "protocol harness exited during Play baseline: {status}; stdout={}; stderr={}; diagnostic={}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr),
                    diagnostic_trace(diagnostic_file)
                );
            }
            if java.connection_state.load() == ConnectionState::Play
                && java.last_packet_time.load() != before_play_packets
                && java.pending_bytes.load(Ordering::Acquire) == 0
            {
                // The harness marker is emitted only after Join Game, position,
                // first chunk, and the real Play keep-alive exchange. Do not add
                // an arbitrary post-marker delay: the harness continues reading
                // bounded Play traffic and must be triggered before that bound.
                if java.connection_state.load() == ConnectionState::Play
                    && java.pending_bytes.load(Ordering::Acquire) == 0
                    && diagnostic_trace(diagnostic_file).contains(required_event)
                {
                    return;
                }
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "timed out waiting for the strict Play baseline/readiness: {error:?}; state={:?}, last_packet_time={:?}, pending_bytes={}, closed={}\ndiagnostic_trace={}",
            java.connection_state.load(),
            java.last_packet_time.load(),
            java.pending_bytes.load(Ordering::Acquire),
            java.is_closed(),
            diagnostic_trace(diagnostic_file),
        )
    });
}

fn inventory_snapshot(player: &pumpkin::entity::player::Player) -> Vec<(u16, u8)> {
    (0..36)
        .map(|slot| {
            let stack = player.inventory.get_slot(slot);
            (stack.item.id, stack.item_count)
        })
        .collect()
}

async fn run_listener(pumpkin_server: Arc<PumpkinServer>) {
    let tasks = Arc::new(TaskTracker::new());
    let mut client_id = 0;
    let bedrock_clients = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    while pumpkin_server
        .unified_listener_task(&mut client_id, &tasks, &bedrock_clients)
        .await
    {}

    tasks.close();
    tasks.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn java_reconfiguration_over_real_tcp_preserves_player_state() {
    SHOULD_STOP.store(false, Ordering::Relaxed);
    assert!(
        !STOP_INTERRUPT.is_cancelled(),
        "global stop token was already cancelled before the integration test"
    );

    let temp_world = TempDir::new().expect("isolated temporary world directory");
    let config = isolated_basic_config(temp_world.path());
    let pumpkin_server = Arc::new(
        PumpkinServer::new(
            config,
            isolated_advanced_config(),
            isolated_telemetry_config(),
            test_vanilla_data(),
        )
        .await,
    );
    let port = pumpkin_server
        .tcp_listener
        .as_ref()
        .expect("Java listener must be enabled")
        .local_addr()
        .expect("loopback listener address")
        .port();
    let listener_task = tokio::spawn(run_listener(pumpkin_server.clone()));
    let diagnostic_dir = TempDir::new().expect("diagnostic directory");
    let normal_diagnostic = diagnostic_dir.path().join("normal.jsonl");
    let eof_diagnostic = diagnostic_dir.path().join("eof.jsonl");
    let egress_trace = Path::new("C:/Temp/pumpkin-26_2-current-egress.log");
    let _ = fs::remove_file(egress_trace);
    // The production writer emits no trace unless this test-only environment
    // switch is set. Keep the first state/epoch/packet boundary outside the repo.
    unsafe {
        std::env::set_var("PUMPKIN_JAVA_EGRESS_TRACE_FILE", &egress_trace);
    }

    let mut normal_harness = spawn_harness(port, TEST_USERNAME, None, &normal_diagnostic);
    let player = wait_for_player(
        &pumpkin_server.server,
        TEST_USERNAME,
        Some(&mut normal_harness),
    )
    .await;
    let java = player
        .client
        .java()
        .expect("real TCP test player must be Java");
    wait_for_state(
        java,
        ConnectionState::Play,
        Some(&mut normal_harness),
        Some(&normal_diagnostic),
    )
    .await;
    // Wait until the real server has drained Join Game/chunks and the harness has
    // published its strict baseline readiness, rather than relying on a synthetic trigger delay.
    wait_for_play_baseline(
        java,
        &normal_diagnostic,
        "play_baseline_ready",
        Some(&mut normal_harness),
    )
    .await;

    let baseline_entity_id = player.entity_id();
    let baseline_position = player.position();
    let baseline_world = player.world();
    let baseline_inventory = inventory_snapshot(&player);

    assert!(
        java.start_reconfiguration().await,
        "first server-owned trigger was rejected: state={:?} version={:?} closed={} pending_bytes={} diagnostic={}",
        java.connection_state.load(),
        java.version.load(),
        java.is_closed(),
        java.pending_bytes.load(Ordering::Acquire),
        diagnostic_trace(&normal_diagnostic)
    );
    assert!(
        !java.start_reconfiguration().await,
        "second trigger was admitted before the first cycle returned to Play"
    );
    wait_for_state(
        java,
        ConnectionState::Config,
        Some(&mut normal_harness),
        Some(&normal_diagnostic),
    )
    .await;
    let config_packet_time = java.last_packet_time.load();
    wait_for_state(
        java,
        ConnectionState::Play,
        Some(&mut normal_harness),
        Some(&normal_diagnostic),
    )
    .await;
    timeout(Duration::from_secs(10), async {
        loop {
            if java.last_packet_time.load() != config_packet_time {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for the first returned Play keep-alive");
    // The state flips to Play while processing the Finish ack. Wait for the
    // harness's fresh return-to-Play readiness event and a drained queue before cycle two.
    wait_for_play_baseline(
        java,
        &normal_diagnostic,
        "returned_play_keepalive",
        Some(&mut normal_harness),
    )
    .await;
    assert!(Arc::ptr_eq(
        &player,
        &pumpkin_server
            .server
            .get_player_by_name(TEST_USERNAME)
            .unwrap_or_else(|| panic!("player must remain server-owned after first cycle: state={:?} closed={} pending_bytes={} diagnostic={} egress={}", java.connection_state.load(), java.is_closed(), java.pending_bytes.load(Ordering::Acquire), diagnostic_trace(&normal_diagnostic), diagnostic_trace(Path::new("C:/Temp/pumpkin-26_2-current-egress.log"))))
    ));

    assert!(
        java.start_reconfiguration().await,
        "second server-owned trigger was rejected"
    );
    assert!(
        !java.start_reconfiguration().await,
        "duplicate second-cycle trigger was admitted"
    );
    wait_for_state(
        java,
        ConnectionState::Config,
        Some(&mut normal_harness),
        Some(&normal_diagnostic),
    )
    .await;
    let second_config_packet_time = java.last_packet_time.load();
    wait_for_state(
        java,
        ConnectionState::Play,
        Some(&mut normal_harness),
        Some(&normal_diagnostic),
    )
    .await;
    timeout(Duration::from_secs(10), async {
        loop {
            if java.last_packet_time.load() != second_config_packet_time {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for the second returned Play keep-alive");

    assert!(Arc::ptr_eq(
        &player,
        &pumpkin_server
            .server
            .get_player_by_name(TEST_USERNAME)
            .expect("same Arc<Player> must remain registered before client EOF")
    ));
    let normal_output = tokio::task::spawn_blocking(|| wait_for_harness(normal_harness))
        .await
        .expect("normal harness wait task panicked");
    let normal_result = parse_harness_output(&normal_output);
    println!("normal_harness_result={normal_result}");
    println!(
        "normal_diagnostic_trace=\n{}",
        diagnostic_trace(&normal_diagnostic)
    );
    println!("server_egress_trace=\n{}", diagnostic_trace(&egress_trace));
    assert_eq!(normal_result["result"], "stateful_reconfiguration_pass");
    assert_eq!(normal_result["cycles_completed"], 2);
    assert_eq!(normal_result["same_entity"], true);
    assert_eq!(normal_result["same_position"], true);
    let events = normal_result["events"]
        .as_array()
        .expect("normal harness must return packet events");
    let event_count = |direction: &str, name: &str| {
        events
            .iter()
            .filter(|event| event["direction"] == direction && event["event"] == name)
            .count()
    };
    assert_eq!(event_count("clientbound", "start_configuration"), 2);
    assert_eq!(event_count("serverbound", "configuration_acknowledged"), 2);
    assert_eq!(event_count("clientbound", "finish_configuration"), 2);
    assert_eq!(event_count("serverbound", "finish_configuration"), 2);
    assert_eq!(event_count("clientbound", "select_known_packs"), 2);
    assert_eq!(event_count("clientbound", "update_tags"), 2);
    assert!(
        event_count("clientbound", "registry_data") >= 2,
        "each Configuration cycle must carry Registry Data"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["validation"] == "needs_review")
            .count(),
        0,
        "normal pass must not hide mapped Configuration packets as needs_review"
    );
    assert!(
        events
            .iter()
            .filter(|event| {
                event["direction"] == "serverbound"
                    && (event["event"] == "configuration_acknowledged"
                        || event["event"] == "finish_configuration")
            })
            .all(|event| event["body_is_empty"] == true),
        "critical acknowledgments must have empty bodies"
    );
    assert_eq!(
        event_count("serverbound", "duplicate_finish_configuration"),
        0
    );
    assert_eq!(
        event_count("clientbound", "join_game"),
        1,
        "Join Game must not be repeated"
    );

    assert_eq!(player.entity_id(), baseline_entity_id);
    assert_eq!(player.position(), baseline_position);
    assert!(Arc::ptr_eq(&player.world(), &baseline_world));
    assert_eq!(inventory_snapshot(&player), baseline_inventory);
    // The harness closes its real socket immediately after its final Play
    // keep-alive, so server-side disconnect cleanup may already have removed the
    // registration here. The pointer assertion above is the ownership check.

    let mut eof_harness = spawn_harness(
        port,
        EOF_USERNAME,
        Some("disconnect_during_config"),
        &eof_diagnostic,
    );
    let eof_player =
        wait_for_player(&pumpkin_server.server, EOF_USERNAME, Some(&mut eof_harness)).await;
    let eof_java = eof_player
        .client
        .java()
        .expect("EOF probe player must be Java");
    wait_for_state(
        eof_java,
        ConnectionState::Play,
        Some(&mut eof_harness),
        Some(&eof_diagnostic),
    )
    .await;
    wait_for_play_baseline(
        eof_java,
        &eof_diagnostic,
        "play_baseline_ready",
        Some(&mut eof_harness),
    )
    .await;
    assert!(
        eof_java.start_reconfiguration().await,
        "EOF probe trigger was rejected: state={:?} version={:?} closed={} pending_bytes={} diagnostic={}",
        eof_java.connection_state.load(),
        eof_java.version.load(),
        eof_java.is_closed(),
        eof_java.pending_bytes.load(Ordering::Acquire),
        diagnostic_trace(&eof_diagnostic)
    );
    wait_for_state(
        eof_java,
        ConnectionState::Config,
        Some(&mut eof_harness),
        Some(&eof_diagnostic),
    )
    .await;

    let eof_output = tokio::task::spawn_blocking(|| wait_for_harness(eof_harness))
        .await
        .expect("EOF harness wait task panicked");
    timeout(Duration::from_secs(15), async {
        loop {
            if eof_java.is_closed()
                && pumpkin_server
                    .server
                    .get_player_by_name(EOF_USERNAME)
                    .is_none()
            {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("server did not observe EOF and remove the EOF probe player");
    let eof_result = parse_harness_output(&eof_output);
    println!("eof_harness_result={eof_result}");
    println!(
        "eof_diagnostic_trace=\n{}",
        diagnostic_trace(&eof_diagnostic)
    );
    println!("server_egress_trace=\n{}", diagnostic_trace(&egress_trace));
    assert_eq!(
        eof_result["result"],
        "client_disconnected_during_configuration"
    );
    assert_eq!(eof_result["peer_shutdown"], "client_eof");
    assert_ne!(
        eof_java.connection_state.load(),
        ConnectionState::Play,
        "EOF during Configuration must not return the client to Play"
    );

    stop_server();
    listener_task.await.expect("server listener task panicked");
    pumpkin_server
        .server
        .shutdown()
        .await
        .expect("isolated server shutdown must complete");
}
