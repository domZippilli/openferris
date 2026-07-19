use std::borrow::Cow;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value};

use super::ChatMessage;

/// Model-neutral generation settings that may need model-specific mapping.
#[derive(Debug, Clone, Copy)]
pub struct GenerationOptions {
    pub enable_thinking: bool,
}

/// A message after a model adapter has translated OpenFerris's conversation
/// semantics into the dialect expected by the configured model.
#[derive(Debug, Serialize)]
pub struct AdaptedMessage<'a> {
    pub role: Cow<'a, str>,
    pub content: Cow<'a, str>,
}

/// Model-specific portions of a chat request. Transport concerns such as the
/// endpoint, streaming, and token limits remain owned by the backend.
#[derive(Debug)]
pub struct AdaptedConversation<'a> {
    pub messages: Vec<AdaptedMessage<'a>>,
    pub chat_template_kwargs: Option<Map<String, Value>>,
}

/// Translate OpenFerris's conversation into a model's chat dialect.
///
/// Adapters must fit the model to OpenFerris's supported conversation
/// semantics. Model/provider features do not belong in `ChatMessage` merely
/// because one backend exposes them.
pub trait ModelAdapter: Send + Sync {
    fn name(&self) -> &'static str;

    fn adapt<'a>(
        &self,
        messages: &'a [ChatMessage],
        options: GenerationOptions,
    ) -> Result<AdaptedConversation<'a>>;
}

fn standard_messages(messages: &[ChatMessage]) -> Vec<AdaptedMessage<'_>> {
    messages
        .iter()
        .map(|message| AdaptedMessage {
            role: Cow::Borrowed(message.role.as_str()),
            content: Cow::Borrowed(message.content.as_str()),
        })
        .collect()
}

/// Standard role/content mapping with no model-specific request extensions.
pub struct GenericModelAdapter;

impl ModelAdapter for GenericModelAdapter {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn adapt<'a>(
        &self,
        messages: &'a [ChatMessage],
        _options: GenerationOptions,
    ) -> Result<AdaptedConversation<'a>> {
        Ok(AdaptedConversation {
            messages: standard_messages(messages),
            chat_template_kwargs: None,
        })
    }
}

/// Gemma 4 mapping for OpenAI-compatible servers such as vLLM.
pub struct Gemma4ModelAdapter;

impl ModelAdapter for Gemma4ModelAdapter {
    fn name(&self) -> &'static str {
        "gemma4"
    }

    fn adapt<'a>(
        &self,
        messages: &'a [ChatMessage],
        options: GenerationOptions,
    ) -> Result<AdaptedConversation<'a>> {
        let chat_template_kwargs = options
            .enable_thinking
            .then(|| Map::from_iter([("enable_thinking".to_string(), Value::Bool(true))]));

        Ok(AdaptedConversation {
            messages: standard_messages(messages),
            chat_template_kwargs,
        })
    }
}

pub fn create_model_adapter(name: &str) -> Result<Box<dyn ModelAdapter>> {
    match name {
        "generic" => Ok(Box::new(GenericModelAdapter)),
        "gemma4" | "gemma-4" => Ok(Box::new(Gemma4ModelAdapter)),
        other => anyhow::bail!("Unknown model adapter {other:?}; expected one of: generic, gemma4"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, Role};

    fn messages() -> Vec<ChatMessage> {
        vec![ChatMessage {
            role: Role::User,
            content: "hello".to_string(),
        }]
    }

    #[test]
    fn generic_adapter_has_no_template_kwargs() {
        let messages = messages();
        let adapted = GenericModelAdapter
            .adapt(
                &messages,
                GenerationOptions {
                    enable_thinking: true,
                },
            )
            .unwrap();

        assert!(adapted.chat_template_kwargs.is_none());
        assert_eq!(adapted.messages[0].role, "user");
        assert_eq!(adapted.messages[0].content, "hello");
    }

    #[test]
    fn gemma4_adapter_enables_thinking_when_requested() {
        let messages = messages();
        let adapted = Gemma4ModelAdapter
            .adapt(
                &messages,
                GenerationOptions {
                    enable_thinking: true,
                },
            )
            .unwrap();

        assert_eq!(
            adapted.chat_template_kwargs.unwrap().get("enable_thinking"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn unknown_adapter_is_rejected() {
        assert!(create_model_adapter("future-model").is_err());
    }
}
