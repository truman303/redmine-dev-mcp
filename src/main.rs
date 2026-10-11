use serde::Deserialize;

use redmine_dev_mcp::server::RedmineServer;
use redmine_dev_mcp::{Client, RedmineError, load};
use rmcp::ServiceExt;

#[derive(Deserialize)]
struct CurrentUser {
    user: User,
}

#[derive(Deserialize)]
struct User {
    id: u64,
    login: Option<String>,
    firstname: Option<String>,
    lastname: Option<String>,
    name: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = match load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };
    let client = match Client::new(config) {
        Ok(client) => client,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };

    // Serve first and check the key alongside. MCP clients time out a slow
    // initialize, and they often launch servers before the network is up, as
    // after a laptop wakes. Only a rejected key is worth exiting over; anything
    // else would leave the tools missing for the whole session.
    let check = client.clone();
    tokio::spawn(async move {
        match ping(&check).await {
            Ok(user) => tracing::info!(
                url = %check.config().base_url,
                user = %user,
                read_only = check.config().read_only,
                "authenticated"
            ),
            Err(err) if err.status() == Some(401) => {
                eprintln!("{err}");
                std::process::exit(1);
            }
            Err(err) => tracing::warn!(
                url = %check.config().base_url,
                "could not reach Redmine yet; tools will retry per call. {err}"
            ),
        }
    });

    let server = RedmineServer::new(client);
    let running = match server.serve(rmcp::transport::stdio()).await {
        Ok(running) => running,
        Err(err) => {
            eprintln!("MCP server failed to start: {err}");
            std::process::exit(1);
        }
    };
    if let Err(err) = running.waiting().await {
        eprintln!("MCP server stopped: {err}");
        std::process::exit(1);
    }
}

async fn ping(client: &Client) -> Result<String, RedmineError> {
    let current = client
        .get_json::<CurrentUser>("/users/current.json", &[])
        .await?;
    Ok(display_user(&current.user))
}

fn display_user(user: &User) -> String {
    if let Some(login) = user.login.as_deref().filter(|login| !login.is_empty()) {
        return format!("{login} #{}", user.id);
    }
    let name = match (&user.firstname, &user.lastname) {
        (Some(first), Some(last)) if !first.is_empty() || !last.is_empty() => {
            format!("{first} {last}")
        }
        _ => user.name.clone().unwrap_or_default(),
    };
    let name = name.trim();
    if name.is_empty() {
        format!("#{}", user.id)
    } else {
        format!("{name} #{}", user.id)
    }
}
