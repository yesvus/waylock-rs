use pam_client2::{
    Context as PamContext, Flag as PamFlag, conv_mock::Conversation as PamConversation,
};

pub(crate) enum AuthResult {
    Success,
    Failure,
}

pub(crate) fn authenticate(username: String, password: String, pam_service: String) -> AuthResult {
    match PamContext::new(
        pam_service.as_str(),
        Some(username.as_str()),
        PamConversation::with_credentials(username.clone(), password),
    ) {
        Ok(mut context) => {
            if context.authenticate(PamFlag::empty()).is_ok() {
                AuthResult::Success
            } else {
                AuthResult::Failure
            }
        }
        Err(_) => AuthResult::Failure,
    }
}
