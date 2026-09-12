//! Lossless conversion between stored preview facts and their wire model.

/// Copy preview facts without rendering or truncating text.
pub fn to_wire(preview: &aj_session::SessionPreview) -> aj_wire::SessionPreview {
    aj_wire::SessionPreview {
        session_id: preview.session_id.clone(),
        modified: preview.modified,
        created_at: preview.created_at,
        last_message_at: preview.last_message_at,
        size_bytes: preview.size_bytes,
        message_count: preview.message_count,
        first_user_message: preview.first_user_message.clone(),
        tag: preview.tag.clone(),
        archived: preview.archived,
    }
}

/// Recover the host's preview facts without applying presentation policy.
pub fn from_wire(preview: aj_wire::SessionPreview) -> aj_session::SessionPreview {
    aj_session::SessionPreview {
        session_id: preview.session_id,
        modified: preview.modified,
        created_at: preview.created_at,
        last_message_at: preview.last_message_at,
        size_bytes: preview.size_bytes,
        message_count: preview.message_count,
        first_user_message: preview.first_user_message,
        tag: preview.tag,
        archived: preview.archived,
    }
}
