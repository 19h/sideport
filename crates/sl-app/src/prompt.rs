use gpui::{App, AppContext, Entity, Window};
use gpui_component::input::InputState;
use sl_engine::{Prompt, PromptKind, PromptReply};

pub(crate) struct PromptDialog {
    pub(crate) prompt: Prompt,
    pub(crate) input: Entity<InputState>,
    pub(crate) remember: bool,
}

impl PromptDialog {
    pub(crate) fn new(prompt: Prompt, window: &mut Window, cx: &mut App) -> Self {
        let masked = matches!(prompt.kind, PromptKind::Password { .. });
        let remember = match prompt.kind {
            PromptKind::Password { remember, .. } => remember,
            _ => false,
        };
        let input = cx.new(|cx| InputState::new(window, cx).masked(masked));

        Self { prompt, input, remember }
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
            PromptKind::WaitForDevice { device_name, reason, .. } => format!("{device_name}: {reason}"),
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
