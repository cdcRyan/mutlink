use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use mutlink::{client::Client, config, model::*, server};
use serde_json::{json, Value};
use std::{
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "mutlink",
    version,
    about = "Your independent terminal agent, across your devices"
)]
struct Cli {
    /// Isolated device identity, configuration and database directory
    #[arg(long, global = true, env = "MUTLINK_HOME")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Open the terminal interface (also the default command)
    Tui,
    /// Run the agent daemon in the foreground
    Serve {
        /// Optional encrypted direct-connection listener, e.g. 0.0.0.0:7447
        #[arg(long)]
        peer_listen: Option<SocketAddr>,
    },
    /// Stop this device's running daemon
    Stop,
    Status,
    /// Configure the model provider
    Model {
        #[command(subcommand)]
        command: Option<ModelCommand>,
    },
    /// Set the authorized workspace (one workspace per device home)
    Workspace {
        path: PathBuf,
    },
    /// Run an agent task, streaming output and requesting approval
    Run {
        prompt: String,
        #[arg(long)]
        plan: bool,
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        session: Option<String>,
        /// Stable request ID, useful when retrying after a connection failure
        #[arg(long)]
        id: Option<String>,
        /// Submit and return immediately; inspect using tasks/events
        #[arg(long)]
        detach: bool,
    },
    Sessions,
    Tasks,
    Events {
        task_id: String,
        #[arg(long)]
        device: Option<String>,
        #[arg(long, default_value_t = 0)]
        after: i64,
    },
    Cancel {
        task_id: String,
        #[arg(long)]
        device: Option<String>,
    },
    Approvals,
    /// Resolve an exact pending approval on this execution device
    Approve {
        id: String,
        #[arg(long, conflicts_with = "deny")]
        allow: bool,
        #[arg(long)]
        deny: bool,
    },
    Devices,
    Device {
        #[arg(long)]
        name: Option<String>,
        /// Reachable base URL for encrypted direct connections
        #[arg(long)]
        endpoint: Option<String>,
    },
    Pair {
        #[command(subcommand)]
        command: PairCommand,
    },
    /// Log in and register this device with a self-hosted relay
    Login {
        #[arg(long)]
        relay: String,
        #[arg(long)]
        account: String,
        #[arg(long)]
        register: bool,
        /// Read password from this environment variable; otherwise prompt without echo
        #[arg(long)]
        password_env: Option<String>,
    },
    Logout,
    /// Run an account/discovery/encrypted-message relay
    Relay {
        #[arg(long, default_value = "127.0.0.1:7448")]
        listen: SocketAddr,
        #[arg(long, default_value = "relay.db")]
        database: PathBuf,
        /// Enable self-registration; disable again after creating accounts
        #[arg(long)]
        allow_registration: bool,
    },
}
#[derive(Subcommand)]
enum ModelCommand {
    /// List model profiles and the editable models.json path
    List,
    /// Select a configured profile as the default for subsequent tasks
    Use { name: String },
    /// Add or update a profile, and select it
    Set {
        /// Profile name used by /model; defaults to the API model name
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        model: String,
        #[arg(long, default_value = "https://api.openai.com/v1")]
        base_url: String,
        #[arg(long, default_value = "OPENAI_API_KEY")]
        api_key_env: String,
        #[arg(long, default_value_t = 24)]
        max_steps: usize,
    },
}
#[derive(Subcommand)]
enum PairCommand {
    /// Export PUBLIC identity only; verify the full ID through another channel
    Export,
    Trust {
        file: PathBuf,
        #[arg(long)]
        fingerprint: String,
        /// Permit this peer to submit agent tasks (approval still required for writes/shell)
        #[arg(long)]
        allow_agent: bool,
    },
    Revoke {
        device_id: String,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("mutlink: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = config::home(cli.home.as_deref())?;
    // Absolute paths remain valid when the daemon outlives the invoking shell.
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()?.join(root)
    };
    if let Some(Command::Serve { peer_listen }) = cli.command {
        return server::serve(&root, peer_listen).await;
    }
    if let Some(Command::Relay {
        listen,
        ref database,
        allow_registration,
    }) = cli.command
    {
        return mutlink::relay::serve(database, listen, allow_registration).await;
    }
    if matches!(cli.command, Some(Command::Stop)) {
        let client = Client::connect(&root).context("No running agent found")?;
        return print(client.call("shutdown", Value::Null, None).await?);
    }
    let client = Client::ensure(&root).await?;
    let value=match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => return mutlink::tui::run(client).await,
        Command::Status => client.call("status",Value::Null,None).await?,
        Command::Model{command} => match command.unwrap_or(ModelCommand::List) {
            ModelCommand::List => client.call("model.list",Value::Null,None).await?,
            ModelCommand::Use{name} => client.call("model.use",json!({"name":name}),None).await?,
            ModelCommand::Set{name,model,base_url,api_key_env,max_steps} =>
                client.call("model.set",json!({"name":name,"model":model,"base_url":base_url,"api_key_env":api_key_env,"max_steps":max_steps}),None).await?,
        },
        Command::Workspace{path} => client.call("workspace.set",json!({"path":path.canonicalize()?}),None).await?,
        Command::Run{prompt,plan,device,session,id:request_id,detach} => {
            let request=RunRequest{id:request_id.unwrap_or_else(id),session_id:session,prompt,plan,delegation_depth:0};
            eprintln!("Task ID: {}",request.id);
            let task: Task=serde_json::from_value(client.call("run",json!(request),device.as_deref()).await?)?;
            if detach {json!(task)} else {return follow(&client,task,device.as_deref()).await;}
        }
        Command::Sessions=>client.call("sessions",Value::Null,None).await?,
        Command::Tasks=>client.call("tasks",Value::Null,None).await?,
        Command::Events{task_id,device,after}=>{
            let task=client.call("task",json!({"task_id":task_id}),device.as_deref()).await?;
            client.call("events",json!({"task_id":task_id,"session_id":task["session_id"],"after":after}),device.as_deref()).await?
        }
        Command::Cancel{task_id,device}=>client.call("cancel",json!({"task_id":task_id}),device.as_deref()).await?,
        Command::Approvals=>client.call("approvals",Value::Null,None).await?,
        Command::Approve{id,allow,deny}=>{
            if !allow && !deny {bail!("Specify --allow or --deny after reviewing `mutlink approvals`");}
            client.call("approve",json!({"id":id,"allow":allow}),None).await?
        }
        Command::Devices=>client.call("devices",Value::Null,None).await?,
        Command::Device{name,endpoint}=>client.call("device.set",json!({"name":name,"endpoint":endpoint}),None).await?,
        Command::Pair{command}=>match command {
            PairCommand::Export=>client.call("pair.export",Value::Null,None).await?,
            PairCommand::Trust{file,fingerprint,allow_agent}=>{
                let device: Device=serde_json::from_slice(&std::fs::read(file)?)?;
                client.call("pair.trust",json!({"device":device,"fingerprint":fingerprint,"allow_agent":allow_agent}),None).await?
            }
            PairCommand::Revoke{device_id}=>client.call("pair.revoke",json!({"id":device_id}),None).await?,
        },
        Command::Login{relay,account,register,password_env}=>{
            let password=if let Some(name)=password_env {std::env::var(name).context("Password environment variable missing")?}
                else {password_prompt()?};
            client.call("login",json!({"url":relay,"account":account,"register":register,"password":password}),None).await?
        }
        Command::Logout=>client.call("logout",Value::Null,None).await?,
        Command::Stop|Command::Serve{..}|Command::Relay{..}=>unreachable!(),
    };
    print(value)
}

fn print(value: Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn password_prompt() -> Result<String> {
    use crossterm::{
        event::{read, Event, KeyCode, KeyModifiers},
        terminal::{disable_raw_mode, enable_raw_mode},
    };
    use std::io::IsTerminal;
    if !io::stdin().is_terminal() {
        bail!("Use --password-env for noninteractive login");
    }
    eprint!("Password: ");
    io::stderr().flush()?;
    enable_raw_mode()?;
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }
    let _guard = Guard;
    let mut password = String::new();
    loop {
        if let Event::Key(key) = read()? {
            match key.code {
                KeyCode::Enter => break,
                KeyCode::Backspace => {
                    password.pop();
                }
                KeyCode::Esc => bail!("Login cancelled"),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    bail!("Login cancelled")
                }
                KeyCode::Char(c) if password.len() < 1024 => password.push(c),
                _ => {}
            }
        }
    }
    eprint!("\r\n");
    Ok(password)
}

async fn follow(client: &Client, mut task: Task, target: Option<&str>) -> Result<()> {
    use std::io::IsTerminal;
    let mut after = 0;
    let mut reviewed = std::collections::HashSet::new();
    loop {
        let events: Vec<Event> = serde_json::from_value(
            client
                .call(
                    "events",
                    json!({"task_id":task.id,"session_id":task.session_id,"after":after}),
                    target,
                )
                .await?,
        )?;
        let full_page = events.len() == 500;
        for event in events {
            after = event.seq;
            match event.kind.as_str() {
                "delta"=>{print!("{}",sanitize(event.data["text"].as_str().unwrap_or("")));io::stdout().flush()?;}
                "tool_start"=>eprintln!("\n› {} {}",event.data["tool"],sanitize(&event.data["arguments"].to_string())),
                "tool_result"=>eprintln!("  {}",sanitize(&event.data["output"].to_string())),
                "approval" if target.is_some()=>eprintln!("\nApproval required on execution device. Open mutlink there or use `mutlink approvals`."),
                _=>{},
            }
        }
        if full_page {
            continue;
        }
        task = serde_json::from_value(
            client
                .call("task", json!({"task_id":task.id}), target)
                .await?,
        )?;
        if target.is_none() && task.state == "waiting_approval" {
            let approvals: Vec<Approval> =
                serde_json::from_value(client.call("approvals", Value::Null, None).await?)?;
            for a in approvals.into_iter().filter(|a| a.task_id == task.id) {
                if !reviewed.insert(a.id.clone()) {
                    continue;
                }
                eprintln!(
                    "\nReview {}:\n{}",
                    a.tool,
                    sanitize(&serde_json::to_string_pretty(&a.arguments)?)
                );
                if io::stdin().is_terminal() {
                    eprint!("Approve this operation? [y/N] ");
                    io::stderr().flush()?;
                    let answer = tokio::task::spawn_blocking(|| {
                        let mut s = String::new();
                        io::stdin().read_line(&mut s).map(|_| s)
                    })
                    .await??;
                    client
                        .call(
                            "approve",
                            json!({"id":a.id,"allow":answer.trim().eq_ignore_ascii_case("y")}),
                            None,
                        )
                        .await?;
                } else {
                    eprintln!(
                        "Waiting for local approval: mutlink approve {} --allow",
                        a.id
                    );
                }
            }
        }
        if !matches!(
            task.state.as_str(),
            "queued" | "running" | "waiting_approval"
        ) {
            // One extra read catches deltas committed between the preceding events and task reads.
            let tail: Vec<Event> = serde_json::from_value(
                client
                    .call(
                        "events",
                        json!({"task_id":task.id,"session_id":task.session_id,"after":after}),
                        target,
                    )
                    .await?,
            )?;
            for e in tail {
                if e.kind == "delta" {
                    print!("{}", sanitize(e.data["text"].as_str().unwrap_or("")));
                }
            }
            println!();
            eprintln!("{} · session {}", task.state, task.session_id);
            if task.state != "succeeded" {
                bail!("{}", task.error.unwrap_or(task.state));
            }
            return Ok(());
        }
        tokio::select! {
            _=tokio::time::sleep(Duration::from_millis(350))=>{},
            _=tokio::signal::ctrl_c()=>{let _=client.call("cancel",json!({"task_id":task.id}),target).await; bail!("Cancelled");}
        }
    }
}
fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
