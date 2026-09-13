//! `im auth register|login|status|logout` — the sync account on this device.

use anyhow::{Result, bail};
use sqlx::SqlitePool;

use crate::cli::AuthSubcommand;
use crate::sync::KEY_LAST_SERVER_VERSION;
use crate::sync::apply;
use crate::sync::client::server_url;
use crate::sync::session;

pub async fn auth_command(pool: &SqlitePool, sub: AuthSubcommand) -> Result<()> {
    match sub {
        AuthSubcommand::Register => {
            let (email, password) = prompt_credentials()?;
            let server = server_url();
            let account = session::register(&server, &email, &password).await?;
            session::store_account(pool, &account).await?;
            println!(
                "Registered {} on {server}; this device is signed in.",
                account.email
            );
            println!("Run `im sync` to upload the events this device has queued.");
        }

        AuthSubcommand::Login => {
            let (email, password) = prompt_credentials()?;
            let server = server_url();
            let account = session::login(&server, &email, &password).await?;
            session::store_account(pool, &account).await?;
            println!("Signed in as {} on {server}.", account.email);
            println!("Run `im sync` to exchange events with the other devices.");
        }

        AuthSubcommand::Status => {
            let Some(token) = session::signed_in(pool).await? else {
                println!("Not signed in (run `im auth login`).");
                return Ok(());
            };
            let server = server_url();
            match session::account_status(&server, &token).await {
                Ok(status) => {
                    let version = session::state_get(pool, KEY_LAST_SERVER_VERSION)
                        .await?
                        .unwrap_or_else(|| "0".to_string());
                    let queued = apply::pending_events(pool).await?.len();
                    println!(
                        "Signed in as {} ({}) on {server}.",
                        status.email, status.user_id
                    );
                    println!("Server version {version}; {queued} event(s) queued to push.");
                }
                Err(error) => {
                    println!("Signed in locally, but {server} rejected the session: {error}")
                }
            }
        }

        AuthSubcommand::Logout => {
            // The server keeps the events it already has; this device keeps
            // its data and simply stops syncing it.
            session::state_remove(pool, crate::sync::KEY_AUTH_TOKEN).await?;
            session::state_remove(pool, crate::sync::KEY_USER_ID).await?;
            session::state_remove(pool, KEY_LAST_SERVER_VERSION).await?;
            println!("Signed out. Local data is untouched.");
        }
    }
    Ok(())
}

fn prompt_credentials() -> Result<(String, String)> {
    if !atty::is(atty::Stream::Stdin) {
        bail!("`im auth` needs an interactive terminal to ask for the email and password");
    }
    let email: String = cliclack::input("Email:")
        .placeholder("you@example.com")
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {e}"))?;
    let password: String = cliclack::password("Password:")
        .mask('*')
        .interact()
        .map_err(|e| anyhow::anyhow!("Prompt cancelled: {e}"))?;
    Ok((email.trim().to_string(), password))
}
