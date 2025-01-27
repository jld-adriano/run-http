use actix_web::{middleware, web, App, HttpResponse, HttpServer, Responder};
use clap::{Parser, ArgAction};
use futures::StreamExt;
use log::{error, info};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::mpsc;
use tokio_stream::wrappers::LinesStream;
use tokio::time::{sleep, Duration};
use actix_web::body::MessageBody;

#[derive(Parser, Debug)]
#[clap(
    author = "jld.adriano@gmail.com",
    version = "0.1.0",
    about = "A CLI tool to run and control commands via http",
    long_about = None
)]
struct Args {
    #[clap(short, long, default_value = "30067", help = "Port to run the web server on")]
    port: u16,

    #[clap(long, default_value = "127.0.0.1", help = "Host address to bind the web server to")]
    host: String,

    #[clap(last = true, required = true, help = "The command to run and monitor")]
    command: Vec<String>,

    #[clap(long, action = ArgAction::SetTrue, hide = true, help = "Generate markdown help")]
    markdown_help: bool,

    #[clap(long, help = "Condition command to run before restarting")]
    restart_condition: Option<String>,

    #[clap(long, action = ArgAction::SetTrue, help = "Ensure restart condition fails at least once before passing")]
    fail_atleast_once: bool,

    #[clap(long, default_value = "300", help = "Sleep duration in milliseconds between restart condition checks")]
    restart_condition_sleep: u64,

    #[clap(long, action = ArgAction::SetTrue, help = "Suppress all logging output, only show command output")]
    quiet: bool,
}

struct AppState {
    command: Vec<String>,
    child_process: Mutex<Option<Child>>,
    output_tx: mpsc::Sender<String>,
    restart_condition: Option<String>,
    fail_atleast_once: bool,
    restart_condition_sleep: u64,
    quiet: bool,
}

async fn run_command(
    command: &[String],
    tx: mpsc::Sender<String>,
) -> Result<Child, std::io::Error> {
    let mut child = TokioCommand::new(&command[0])
        .args(&command[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    tokio::spawn(stream_output(
        LinesStream::new(BufReader::new(stdout).lines()),
        tx.clone(),
    ));
    tokio::spawn(stream_output(
        LinesStream::new(BufReader::new(stderr).lines()),
        tx.clone(),
    ));

    Ok(child)
}

async fn stream_output<S>(mut stream: S, tx: mpsc::Sender<String>)
where
    S: StreamExt<Item = Result<String, std::io::Error>> + Unpin,
{
    while let Some(line) = stream.next().await {
        if let Ok(line) = line {
            let _ = tx.send(line).await;
        }
    }
}

async fn start_command(data: web::Data<Arc<AppState>>) -> Result<HttpResponse, actix_web::Error> {
    if !data.quiet {
        info!("Received request to start command");
    }
    let command = data.command.clone();
    let already_running = data.child_process.lock().unwrap().is_some();
    if already_running {
        if !data.quiet {
            info!("Command is already running");
        }
        return Ok(HttpResponse::BadRequest().body("Command is already running"));
    }

    match run_command(&command, data.output_tx.clone()).await {
        Ok(child) => {
            *data.child_process.lock().unwrap() = Some(child);
            if !data.quiet {
                info!("Command started successfully, monitoring child");
                let _ = data
                    .output_tx
                    .send("Command started successfully".to_string())
                    .await;
            }
            tokio::spawn(monitor_child(data.clone()));
            Ok(HttpResponse::Ok().body("Command started successfully"))
        }
        Err(e) => {
            if !data.quiet {
                error!("Failed to start command: {}", e);
                let _ = data
                    .output_tx
                    .send(format!("Failed to start command: {}", e))
                    .await;
            }
            Ok(HttpResponse::InternalServerError().body(format!("Failed to start command: {}", e)))
        }
    }
}

async fn monitor_child(data: web::Data<Arc<AppState>>) {
    let child = {
        let mut guard = data.child_process.lock().unwrap();
        if !data.quiet {
            info!("Monitor - Getting child process, current state: {:?}", guard.is_some());
        }
        guard.as_mut().map(|child| child.id())
    };
    
    match child {
        Some(pid) => {
            if !data.quiet {
                info!("Monitor - Found child process with PID: {:?}", pid);
            }
            
            let mut final_status = None;
            let mut last_log = std::time::Instant::now();
            
            loop {
                let should_break = {
                    let mut guard = data.child_process.lock().unwrap();
                    if let Some(child) = guard.as_mut() {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                if !data.quiet {
                                    info!("Monitor - Child process exited with status: {:?}", status);
                                }
                                *guard = None;
                                final_status = Some(status);
                                true
                            }
                            Ok(None) => {
                                let now = std::time::Instant::now();
                                if !data.quiet && now.duration_since(last_log).as_secs() >= 2 {
                                    info!("Monitor - Child process still running");
                                    last_log = now;
                                }
                                false
                            }
                            Err(e) => {
                                if !data.quiet {
                                    error!("Monitor - Error checking process status: {}", e);
                                }
                                false
                            }
                        }
                    } else {
                        if !data.quiet {
                            info!("Monitor - Child process no longer in state");
                        }
                        true
                    }
                };

                if should_break {
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
            
            if !data.quiet {
                info!("Monitor - Finished monitoring process with final status: {:?}", final_status);
            }

            if let Some(status) = final_status {
                if !status.success() {
                    let _ = data
                        .output_tx
                        .send(format!("Command exited with error: {:?}", status))
                        .await;
                }
            }
        }
        None => {
            if !data.quiet {
                info!("Monitor - No child process found to monitor");
            }
            let msg = "Error: No child process found to monitor".to_string();
            if !data.quiet {
                error!("{}", msg);
            }
            let _ = data.output_tx.send(msg).await;
        }
    }
}

async fn run_restart_condition(condition: &str) -> bool {
    let output = TokioCommand::new("sh")
        .arg("-c")
        .arg(condition)
        .output()
        .await;

    match output {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

async fn restart_command(data: web::Data<Arc<AppState>>) -> impl Responder {
    if !data.quiet {
        info!("Received request to restart command");
    }
    
    if let Some(condition) = &data.restart_condition {
        let mut has_failed = !data.fail_atleast_once;
        loop {
            if !data.quiet {
                info!("Checking restart condition: {}", condition);
            }
            let condition_result = run_restart_condition(condition).await;
            if !condition_result {
                has_failed = true;
            }
            if condition_result && (!data.fail_atleast_once || has_failed) {
                if !data.quiet {
                    info!("Restart condition met");
                }
                break;
            }
            sleep(Duration::from_millis(data.restart_condition_sleep)).await;
        }
    }

    let mut child_process = data.child_process.lock().unwrap();
    if let Some(mut child) = child_process.take() {
        if !data.quiet {
            info!("Killing existing child process");
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    match run_command(&data.command, data.output_tx.clone()).await {
        Ok(child) => {
            *child_process = Some(child);
            tokio::spawn(monitor_child(data.clone()));
            if !data.quiet {
                info!("Command restarted successfully");
            }
            HttpResponse::Ok().body("Command restarted successfully")
        }
        Err(e) => {
            if !data.quiet {
                error!("Failed to restart command: {}", e);
            }
            HttpResponse::InternalServerError().body(format!("Failed to restart command: {}", e))
        }
    }
}

async fn stop_command(data: web::Data<Arc<AppState>>) -> impl Responder {
    if !data.quiet {
        info!("Stop - Received request to stop command");
    }
    let mut child_process = data.child_process.lock().unwrap();
    if !data.quiet {
        info!("Stop - Got lock, child process state: {:?}", child_process.is_some());
    }
    
    if let Some(mut child) = child_process.take() {
        if !data.quiet {
            info!("Stop - Found child process, attempting to kill");
            let pid = child.id();
            info!("Stop - Child PID: {:?}", pid);
        }
        
        let kill_result = child.kill().await.is_ok();
        if !data.quiet {
            info!("Stop - Kill result: {}", kill_result);
        }
        
        let wait_result = child.wait().await;
        if !data.quiet {
            info!("Stop - Wait result: {:?}", wait_result);
            info!("Stop - Command stopped successfully");
        }
        HttpResponse::Ok().body("Command stopped successfully")
    } else {
        if !data.quiet {
            info!("Stop - No command is running");
        }
        HttpResponse::BadRequest().body("No command is running")
    }
}

async fn get_status(data: web::Data<Arc<AppState>>) -> impl Responder {
    if !data.quiet {
        info!("Received request to get command status");
    }
    let child_process = data.child_process.lock().unwrap();
    if child_process.is_some() {
        if !data.quiet {
            info!("Command is running");
        }
        HttpResponse::Ok().body("Command is running")
    } else {
        if !data.quiet {
            info!("Command is not running");
        }
        HttpResponse::Ok().body("Command is not running")
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();

    if args.markdown_help {
        clap_markdown::print_help_markdown::<Args>();
        println!("# Examples\n");
        println!("```");
        println!("run-http -- python -c 'import time; i=0; while True: print(f\"Count: {{i}}\"); i+=1; time.sleep(1)'");
        println!("curl http://localhost:30067/start");
        println!("curl http://localhost:30067/status");
        println!("curl http://localhost:30067/stop");
        println!("curl http://localhost:30067/restart");
        println!("```");
        return Ok(());
    }

    if !args.quiet {
        env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
        info!("\n\n\nBooting up...");
    }

    let (tx, mut rx) = mpsc::channel(100);

    let app_state = Arc::new(AppState {
        command: args.command.clone(),
        child_process: Mutex::new(None),
        output_tx: tx.clone(),
        restart_condition: args.restart_condition,
        fail_atleast_once: args.fail_atleast_once,
        restart_condition_sleep: args.restart_condition_sleep,
        quiet: args.quiet,
    });

    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if args.quiet {
                print!("{}", line);  // Direct passthrough in quiet mode
            } else {
                println!("Output: {}", line);
            }
        }
    });

    // Start the command on boot
    match run_command(&args.command, tx.clone()).await {
        Ok(child) => {
            *app_state.child_process.lock().unwrap() = Some(child);
            if !args.quiet {
                info!("Command started successfully on boot");
                let _ = tx
                    .send("Command started successfully on boot".to_string())
                    .await;
            }
            tokio::spawn(monitor_child(web::Data::new(app_state.clone())));
        }
        Err(e) => {
            if !args.quiet {
                error!("Failed to start command on boot: {}", e);
                let _ = tx
                    .send(format!("Failed to start command on boot: {}", e))
                    .await;
            }
        }
    }

    let bind_address = format!("{}:{}", args.host, args.port);
    if !args.quiet {
        info!("Attempting to bind to http://{}", bind_address);
    }

    match HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(app_state.clone()))
            .wrap(middleware::Logger::new(
                "%a \"%r\" %s %b \"%{Referer}i\" \"%{User-Agent}i\" %T",
            ))
            .route("/start", web::get().to(start_command))
            .route("/restart", web::get().to(restart_command))
            .route("/stop", web::get().to(stop_command))
            .route("/status", web::get().to(get_status))
    })
    .bind(&bind_address)
    {
        Ok(server) => {
            if !args.quiet {
                info!("Server successfully bound to http://{}", bind_address);
            }
            server.run().await
        }
        Err(e) => {
            if !args.quiet {
                error!("Failed to bind to {}: {}", bind_address, e);
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;
    use actix_web::test::TestRequest;

    // Helper function to get response body as string
    async fn get_body_as_string(resp: impl Responder) -> String {
        let req = TestRequest::get().to_http_request();
        let response = resp.respond_to(&req);
        if let Ok(bytes) = response.into_body().try_into_bytes() {
            String::from_utf8(bytes.to_vec()).unwrap_or_default()
        } else {
            String::new()
        }
    }

    async fn setup_test_state() -> web::Data<Arc<AppState>> {
        let (tx, _rx) = mpsc::channel(100);
        let state = Arc::new(AppState {
            command: vec!["echo".to_string(), "test".to_string()],
            child_process: Mutex::new(None),
            output_tx: tx,
            restart_condition: None,
            fail_atleast_once: false,
            restart_condition_sleep: 100,
            quiet: false,
        });
        web::Data::new(state)
    }

    #[test]
    fn test_start_stop_command() {
        let rt = create_runtime();
        rt.block_on(async {
            let state = setup_test_state().await;
            
            info!("Test - Starting long running command");
            let command = vec!["sleep".to_string(), "2".to_string()];
            let child = run_command(&command, state.output_tx.clone())
                .await
                .expect("Failed to start sleep command");
            
            info!("Test - Setting child process");
            {
                let mut lock = state.child_process.lock().unwrap();
                info!("Test - Got lock, current state: {:?}", lock.is_some());
                *lock = Some(child);
                info!("Test - Child process set, new state: {:?}", lock.is_some());
            }
            
            info!("Test - Spawning monitor");
            tokio::spawn(monitor_child(state.clone()));
            
            // Give the command a moment to start
            info!("Test - Waiting for command to start");
            sleep(Duration::from_millis(100)).await;
            
            // Verify command is running
            info!("Test - Checking status");
            {
                let lock = state.child_process.lock().unwrap();
                info!("Test - Child process state before status check: {:?}", lock.is_some());
            }
            let status_resp = get_status(state.clone()).await;
            let status_body = get_body_as_string(status_resp).await;
            info!("Test - Status response: {}", status_body);
            assert!(status_body.contains("running"), "Command should be running");
            
            // Test stopping
            info!("Test - Attempting to stop command");
            {
                let lock = state.child_process.lock().unwrap();
                info!("Test - Child process state before stop: {:?}", lock.is_some());
            }
            let stop_resp = stop_command(state.clone()).await;
            let stop_body = get_body_as_string(stop_resp).await;
            info!("Test - Stop command response: {}", stop_body);
            assert!(stop_body.contains("successfully"), "Stop command failed: {}", stop_body);
            
            // Give the command a moment to stop
            info!("Test - Waiting for command to stop");
            sleep(Duration::from_millis(100)).await;
            
            // Verify command is stopped
            info!("Test - Verifying command is stopped");
            {
                let lock = state.child_process.lock().unwrap();
                info!("Test - Child process state before final check: {:?}", lock.is_some());
            }
            let status_resp = get_status(state.clone()).await;
            let status_body = get_body_as_string(status_resp).await;
            info!("Test - Final status: {}", status_body);
            assert!(status_body.contains("not running"), "Command should not be running");
        });
    }

    #[test]
    fn test_restart_with_condition() {
        let rt = create_runtime();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(100);
            
            let state = Arc::new(AppState {
                command: vec!["echo".to_string(), "test".to_string()],
                child_process: Mutex::new(None),
                output_tx: tx,
                restart_condition: Some("exit 0".to_string()),
                fail_atleast_once: false,
                restart_condition_sleep: 100,
                quiet: false,
            });
            let state = web::Data::new(state);

            // Start command
            let _ = start_command(state.clone()).await;
            
            // Test restart
            let restart_body = get_body_as_string(restart_command(state.clone()).await).await;
            assert!(restart_body.contains("successfully"));

            // Check output
            while let Ok(msg) = rx.try_recv() {
                if msg.contains("successfully") {
                    return;
                }
            }
        });
    }

    #[test]
    fn test_long_running_command() {
        let rt = create_runtime();
        rt.block_on(async {
            let state = setup_test_state().await;
            
            // Start a long-running command
            let command = vec!["sleep".to_string(), "1".to_string()];
            let child = run_command(&command, state.output_tx.clone())
                .await
                .expect("Failed to start sleep command");
            
            *state.child_process.lock().unwrap() = Some(child);
            tokio::spawn(monitor_child(state.clone()));

            // Give the command a moment to start
            sleep(Duration::from_millis(100)).await;

            // Verify it's running
            let status_body = get_body_as_string(get_status(state.clone()).await).await;
            assert!(status_body.contains("running"), "Command should be running initially");

            // Wait for finish
            sleep(Duration::from_millis(1200)).await;  // Wait longer than the sleep command

            let status_body = get_body_as_string(get_status(state.clone()).await).await;
            assert!(status_body.contains("not running"), "Command should have finished");
        });
    }

    #[test]
    fn test_cli_args() {
        let args = Args::parse_from([
            "run-http",
            "--port",
            "8080",
            "--host",
            "127.0.0.1",
            "--",
            "echo",
            "hello"
        ]);

        assert_eq!(args.port, 8080);
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.command, vec!["echo", "hello"]);
        assert_eq!(args.fail_atleast_once, false);
        assert_eq!(args.restart_condition_sleep, 300);
        assert_eq!(args.restart_condition, None);
    }

    #[test]
    fn test_cli_restart_condition() {
        let args = Args::parse_from([
            "run-http",
            "--restart-condition",
            "test -f file.txt",
            "--fail-atleast-once",
            "--restart-condition-sleep",
            "100",
            "--",
            "echo",
            "hello"
        ]);

        assert_eq!(args.restart_condition.unwrap(), "test -f file.txt");
        assert_eq!(args.fail_atleast_once, true);
        assert_eq!(args.restart_condition_sleep, 100);
    }

    #[test]
    fn test_cli_quiet_flag() {
        let args = Args::parse_from([
            "run-http",
            "--quiet",
            "--",
            "echo",
            "hello"
        ]);

        assert_eq!(args.quiet, true);
        assert_eq!(args.command, vec!["echo", "hello"]);
    }

    #[test]
    fn test_quiet_state_propagation() {
        let rt = create_runtime();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(100);
            
            let state = Arc::new(AppState {
                command: vec!["echo".to_string(), "test".to_string()],
                child_process: Mutex::new(None),
                output_tx: tx,
                restart_condition: None,
                fail_atleast_once: false,
                restart_condition_sleep: 100,
                quiet: true,
            });
            let state = web::Data::new(state);

            // Start command and verify output is still sent despite quiet mode
            let start_resp = start_command(state.clone()).await.unwrap();
            let start_body = get_body_as_string(start_resp).await;
            assert!(start_body.contains("successfully"), "Command should start successfully even in quiet mode");

            // Check that command output is received
            while let Ok(msg) = rx.try_recv() {
                if msg.contains("test") {
                    return; // Test passed - we got command output in quiet mode
                }
            }
        });
    }

    fn create_runtime() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }
}