use gpui::{App, AppContext, Entity, Window};
use gpui_component::input::InputState;
use sl_engine::{Prompt, PromptKind, PromptReply, TeamKind};

/// A job question shown in the window's modal dialog.
pub(crate) struct PromptDialog {
    pub(crate) prompt: Prompt,
    pub(crate) input: Entity<InputState>,
    pub(crate) remember: bool,
    /// The input hides its characters (passwords).
    pub(crate) masked: bool,
    /// A display name for a device question, when the device list knows the UDID.
    pub(crate) device_name: Option<String>,
}

impl PromptDialog {
    pub(crate) fn new(prompt: Prompt, device_name: Option<String>, window: &mut Window, cx: &mut App) -> Self {
        let masked = matches!(prompt.kind, PromptKind::Password { .. });
        let remember = match prompt.kind {
            PromptKind::Password { remember, .. } => remember,
            _ => false,
        };
        let placeholder = match prompt.kind {
            PromptKind::Password { .. } => "Password",
            PromptKind::SecondFactor { .. } => "Verification code",
            PromptKind::SaveFile { .. } => "Output file path",
            _ => "",
        };

        let input = cx.new(|cx| InputState::new(window, cx).masked(masked).placeholder(placeholder));

        Self { prompt, input, remember, masked, device_name }
    }

    pub(crate) fn title(&self) -> &str {
        match &self.prompt.kind {
            PromptKind::Password { .. } => "Sign in to Apple",
            PromptKind::SecondFactor { .. } => "Verification code",
            PromptKind::ChooseTeam { .. } => "Choose a development team",
            PromptKind::Confirm { title, .. } => title,
            PromptKind::SaveFile { .. } => "Export IPA",
            PromptKind::WaitForDevice { .. } => "Reconnect your device",
        }
    }

    pub(crate) fn message(&self) -> String {
        match &self.prompt.kind {
            PromptKind::Password { apple_id, .. } => format!("Enter the password for {apple_id}."),
            PromptKind::SecondFactor { destination, code_length, .. } => {
                format!("Enter the {code_length}-digit code sent to {destination}.")
            }
            PromptKind::ChooseTeam { apple_id, .. } => format!("Select the team to use with {apple_id}."),
            PromptKind::Confirm { message, .. } => message.clone(),
            PromptKind::SaveFile { suggested_name } => format!("Choose where to save {suggested_name}."),
            PromptKind::WaitForDevice { udid, reason, .. } => {
                let name = self.device_name.as_deref().unwrap_or(udid);

                format!("{name} is no longer connected. {reason}")
            }
        }
    }

    /// Whether the dialog shows the text input.
    pub(crate) fn has_input(&self) -> bool {
        matches!(
            self.prompt.kind,
            PromptKind::Password { .. } | PromptKind::SecondFactor { .. } | PromptKind::SaveFile { .. }
        )
    }

    /// Label of the default button; `None` when the answer is chosen from a list.
    pub(crate) fn primary_label(&self) -> Option<String> {
        match &self.prompt.kind {
            PromptKind::Password { .. } => Some("Sign in".into()),
            PromptKind::SecondFactor { .. } => Some("Verify".into()),
            PromptKind::ChooseTeam { .. } => None,
            PromptKind::Confirm { confirm_label, .. } => Some(confirm_label.clone()),
            PromptKind::SaveFile { .. } => Some("Save".into()),
            PromptKind::WaitForDevice { .. } => Some("Retry now".into()),
        }
    }

    /// Destructive questions style the confirm button as dangerous and ignore Enter.
    pub(crate) fn destructive(&self) -> bool {
        matches!(self.prompt.kind, PromptKind::Confirm { destructive: true, .. })
    }

    /// The answer for the dialog's cancel button and Escape: confirmations are declined,
    /// every other question cancels the job's request.
    pub(crate) fn dismissal(&self) -> PromptReply {
        match self.prompt.kind {
            PromptKind::Confirm { .. } => PromptReply::Confirmed(false),
            _ => PromptReply::Cancel,
        }
    }

    pub(crate) fn reply(&self, cx: &App) -> Result<PromptReply, String> {
        let value = self.input.read(cx).value().to_string();

        match &self.prompt.kind {
            PromptKind::Password { .. } => {
                if value.is_empty() {
                    return Err("Enter your password.".into());
                }

                Ok(PromptReply::Text { value, remember: self.remember })
            }
            PromptKind::SecondFactor { code_length, .. } => {
                if value.len() != *code_length || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(format!("Enter exactly {code_length} digits."));
                }

                Ok(PromptReply::Text { value, remember: false })
            }
            PromptKind::SaveFile { .. } => {
                if value.trim().is_empty() {
                    return Err("Choose an output file.".into());
                }

                Ok(PromptReply::Path(value.into()))
            }
            PromptKind::Confirm { .. } | PromptKind::WaitForDevice { .. } => Ok(PromptReply::Confirmed(true)),
            PromptKind::ChooseTeam { .. } => Err("Select a team.".into()),
        }
    }
}

pub(crate) fn team_kind(kind: &TeamKind) -> &str {
    match kind {
        TeamKind::Free => "Free",
        TeamKind::Individual => "Individual",
        TeamKind::Organization => "Organization",
        TeamKind::Other(name) => name,
    }
}
