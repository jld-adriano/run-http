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
}

struct AppState {
    command: Vec<String>,
    child_process: Mutex<Option<Child>>,
    output_tx: mpsc::Sender<String>,
    restart_condition: Option<String>,
    fail_atleast_once: bool,
    restart_condition_sleep: u64,
}

async fn run_command(
    command: &[String],
    tx: mpsc::Sender<String>,
) -> Result<Child, std::io::Error> {
    println!("Running command: {:?}", command);
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
    info!("Received request to start command");
    let command = data.command.clone();
    let already_running = data.child_process.lock().unwrap().is_some();
    if already_running {
        info!("Command is already running");
        return Ok(HttpResponse::BadRequest().body("Command is already running"));
    }

    match run_command(&command, data.output_tx.clone()).await {
        Ok(child) => {
            *data.child_process.lock().unwrap() = Some(child);
            info!("Command started successfully, monitoring child");
            tokio::spawn(monitor_child(data.clone()));
            let _ = data
                .output_tx
                .send("Command started successfully".to_string())
                .await;
            Ok(HttpResponse::Ok().body("Command started successfully"))
        }
        Err(e) => {
            error!("Failed to start command: {}", e);
            let _ = data
                .output_tx
                .send(format!("Failed to start command: {}", e))
                .await;
            Ok(HttpResponse::InternalServerError().body(format!("Failed to start command: {}", e)))
        }
    }
}

async fn monitor_child(data: web::Data<Arc<AppState>>) {
    let mut child = data.child_process.lock().unwrap().take().unwrap();
    let status = child.wait().await.unwrap();
    println!("Command exited with status: {:?}", status);
    if !status.success() {
        let _ = data
            .output_tx
            .send(format!("Command exited with error: {:?}", status))
            .await;
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
    info!("Received request to restart command");
    
    if let Some(condition) = &data.restart_condition {
        let mut has_failed = !data.fail_atleast_once;
        loop {
            info!("Checking restart condition: {}", condition);
            let condition_result = run_restart_condition(condition).await;
            if !condition_result {
                has_failed = true;
            }
            if condition_result && (!data.fail_atleast_once || has_failed) {
                info!("Restart condition met");
                break;
            }
            sleep(Duration::from_millis(data.restart_condition_sleep)).await;
        }
    }

    let mut child_process = data.child_process.lock().unwrap();
    if let Some(mut child) = child_process.take() {
        info!("Killing existing child process");
        let _ = child.kill();
        let _ = child.wait();
    }
    match run_command(&data.command, data.output_tx.clone()).await {
        Ok(child) => {
            *child_process = Some(child);
            tokio::spawn(monitor_child(data.clone()));
            info!("Command restarted successfully");
            HttpResponse::Ok().body("Command restarted successfully")
        }
        Err(e) => {
            error!("Failed to restart command: {}", e);
            HttpResponse::InternalServerError().body(format!("Failed to restart command: {}", e))
        }
    }
}

async fn stop_command(data: web::Data<Arc<AppState>>) -> impl Responder {
    info!("Received request to stop command");
    let mut child_process = data.child_process.lock().unwrap();
    if let Some(mut child) = child_process.take() {
        info!("Killing child process");
        let _ = child.kill();
        let _ = child.wait();
        info!("Command stopped successfully");
        HttpResponse::Ok().body("Command stopped successfully")
    } else {
        info!("No command is running");
        HttpResponse::BadRequest().body("No command is running")
    }
}

async fn get_status(data: web::Data<Arc<AppState>>) -> impl Responder {
    info!("Received request to get command status");
    let child_process = data.child_process.lock().unwrap();
    if child_process.is_some() {
        info!("Command is running");
        HttpResponse::Ok().body("Command is running")
    } else {
        info!("Command is not running");
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

    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    info!("\n\n\nBooting up...");
    let (tx, mut rx) = mpsc::channel(100);

    let app_state = Arc::new(AppState {
        command: args.command.clone(),
        child_process: Mutex::new(None),
        output_tx: tx.clone(),
        restart_condition: args.restart_condition,
        fail_atleast_once: args.fail_atleast_once,
        restart_condition_sleep: args.restart_condition_sleep,
    });

    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            println!("Output: {}", line);
        }
    });

    // Start the command on boot
    match run_command(&args.command, tx.clone()).await {
        Ok(child) => {
            *app_state.child_process.lock().unwrap() = Some(child);
            info!("Command started successfully on boot");
            let _ = tx
                .send("Command started successfully on boot".to_string())
                .await;
            tokio::spawn(monitor_child(web::Data::new(app_state.clone())));
        }
        Err(e) => {
            error!("Failed to start command on boot: {}", e);
            let _ = tx
                .send(format!("Failed to start command on boot: {}", e))
                .await;
        }
    }

    let bind_address = format!("{}:{}", args.host, args.port);
    info!("Attempting to bind to http://{}", bind_address);

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
            info!("Server successfully bound to http://{}", bind_address);
            server.run().await
        }
        Err(e) => {
            error!("Failed to bind to {}: {}", bind_address, e);
            Err(e)
        }
    }
}