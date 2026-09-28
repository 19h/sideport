//! Running engine jobs: progress, facts and interactive prompts on the terminal.

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use sl_engine::{Fact, JobEvent, JobHandle, LogLevel, Prompt, PromptKind, PromptReply};
use std::io::IsTerminal;
use std::sync::Once;
use tokio_util::sync::CancellationToken;

/// Answers for prompts that the command line supplied in advance.
#[derive(Debug, Clone, Default)]
pub struct Answers {
    /// Default of the password prompt's remember choice.
    pub remember: bool,
}

/// Route SIGINT to the current job. The handler is installed once per process.
fn cancel_on_interrupt(token: CancellationToken) -> Result<()> {
    static INSTALLED: Once = Once::new();
    static CURRENT: std::sync::Mutex<Option<CancellationToken>> = std::sync::Mutex::new(None);

    *CURRENT.lock().expect("cancellation slot") = Some(token);

    let mut installed = Ok(());

    INSTALLED.call_once(|| {
        installed = ctrlc::set_handler(|| {
            if let Some(token) = CURRENT.lock().expect("cancellation slot").as_ref() {
                token.cancel();
            }
        });
    });

    installed.context("register cancellation handler")
}

/// Drive a job to completion, printing stages, logs and facts to stderr.
pub fn run<T>(job: JobHandle<T>, answers: &Answers) -> Result<T> {
    let events = job.events();
    cancel_on_interrupt(job.cancellation_token())?;

    let progress = if std::io::stderr().is_terminal() { ProgressBar::new_spinner() } else { ProgressBar::hidden() };
    progress
        .set_style(ProgressStyle::with_template("{spinner:.cyan} {msg} {wide_bar:.cyan/blue} {bytes}/{total_bytes}")?);
    progress.enable_steady_tick(std::time::Duration::from_millis(100));

    let outcome = futures::executor::block_on(async {
        while let Ok(event) = events.recv().await {
            match event {
                JobEvent::Stage(stage) => {
                    if progress.is_hidden() {
                        eprintln!("{}", stage.label());
                    }

                    progress.set_message(stage.label());
                    progress.set_length(0);
                    progress.set_position(0);
                }

                JobEvent::Progress { done, total } => {
                    progress.set_length(total);
                    progress.set_position(done);
                }

                JobEvent::Log { level, message } if level >= LogLevel::Info => print_line(&progress, &message),

                JobEvent::Fact(fact) => {
                    if let Some(line) = describe(&fact) {
                        print_line(&progress, &line);
                    }
                }

                JobEvent::Prompt(prompt) => {
                    let reply = progress.suspend(|| answer(&prompt, answers));
                    prompt.answer(reply);
                }

                _ => {}
            }
        }

        job.result().await
    });

    progress.finish_and_clear();

    Ok(outcome?)
}

fn print_line(progress: &ProgressBar, line: &str) {
    if progress.is_hidden() { eprintln!("{line}") } else { progress.println(line) }
}

fn describe(fact: &Fact) -> Option<String> {
    match fact {
        Fact::BundleId(identifier) => Some(format!("Bundle identifier: {identifier}")),
        Fact::Team(team) => Some(format!("Team: {} ({})", team.name, team.team_id)),
        Fact::AppIdQuota { remaining, next_release } => Some(match next_release {
            Some(release) => {
                format!("App IDs available: {remaining}; next release {}", release.format("%Y-%m-%d %H:%M UTC"))
            }
            None => format!("App IDs available: {remaining}"),
        }),
        Fact::ProfileExpiry { expires, ttl_days } => Some(format!(
            "Profile expires {}{}",
            expires.format("%Y-%m-%d %H:%M UTC"),
            ttl_days.map(|days| format!(" ({days} days)")).unwrap_or_default()
        )),
        Fact::AnisetteDevice(description) => Some(format!("Apple sees this machine as {description}")),
        Fact::EncryptedBinary => None,
    }
}

/// Interactive answers when stdin is a terminal; otherwise decline, so unattended runs fail
/// instead of waiting.
fn answer(prompt: &Prompt, answers: &Answers) -> PromptReply {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "An interactive answer is required ({}); run in a terminal or pass the value as an option.",
            label(&prompt.kind)
        );

        return match prompt.kind {
            PromptKind::Confirm { .. } => PromptReply::Confirmed(false),
            _ => PromptReply::Cancel,
        };
    }

    let reply = match &prompt.kind {
        PromptKind::Password { apple_id, .. } => dialoguer::Password::new()
            .with_prompt(format!("Password for {apple_id}"))
            .interact()
            .map(|value| PromptReply::Text { value, remember: answers.remember }),

        PromptKind::SecondFactor { destination, code_length, can_request_sms, .. } => {
            let hint = if *can_request_sms { ", or \"sms\" for a text message" } else { "" };

            dialoguer::Input::<String>::new()
                .with_prompt(format!("{code_length}-digit code sent to {destination}{hint}"))
                .interact_text()
                .map(|value| match value.trim() {
                    "sms" if *can_request_sms => PromptReply::RequestSms,
                    "" => PromptReply::Cancel,
                    code => PromptReply::Text { value: code.into(), remember: false },
                })
        }

        PromptKind::ChooseTeam { apple_id, teams } => {
            let items: Vec<_> = teams
                .iter()
                .map(|choice| format!("{} ({}, {:?})", choice.team.name, choice.team.team_id, choice.team.kind))
                .collect();

            dialoguer::Select::new()
                .with_prompt(format!("Team for {apple_id}"))
                .items(&items)
                .default(0)
                .interact()
                .map(PromptReply::Choice)
        }

        PromptKind::Confirm { title, message, confirm_label, destructive } => dialoguer::Confirm::new()
            .with_prompt(format!("{title}\n{message}\n{confirm_label}?"))
            .default(!destructive)
            .interact()
            .map(PromptReply::Confirmed),

        PromptKind::SaveFile { suggested_name } => dialoguer::Input::<String>::new()
            .with_prompt("Export IPA to")
            .default(suggested_name.clone())
            .interact_text()
            .map(|value| PromptReply::Path(value.into())),

        PromptKind::WaitForDevice { reason, .. } => dialoguer::Confirm::new()
            .with_prompt(format!("{reason}\nRetry now?"))
            .default(true)
            .interact()
            .map(|retry| if retry { PromptReply::Confirmed(true) } else { PromptReply::Cancel }),
    };

    reply.unwrap_or(PromptReply::Cancel)
}

fn label(kind: &PromptKind) -> &'static str {
    match kind {
        PromptKind::Password { .. } => "password",
        PromptKind::SecondFactor { .. } => "verification code",
        PromptKind::ChooseTeam { .. } => "team choice",
        PromptKind::Confirm { .. } => "confirmation",
        PromptKind::SaveFile { .. } => "output path",
        PromptKind::WaitForDevice { .. } => "device reconnection",
    }
}
