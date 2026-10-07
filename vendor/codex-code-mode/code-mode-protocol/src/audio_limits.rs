// Modified for the standalone AJ extraction: retain only the shared audio limit.
/// Maximum accepted decoded byte length for prompt audio inputs.
///
/// This matches the Responses API audio input limit.
pub const MAX_PROMPT_AUDIO_INPUT_BYTES: usize = 50 * 1024 * 1024;
