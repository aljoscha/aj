//! Lossless conversion from stored preview facts to their wire model.

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
