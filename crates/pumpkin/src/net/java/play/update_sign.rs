#[allow(clippy::wildcard_imports)]
use super::*;

fn has_editor_grant(editor: Option<uuid::Uuid>, sender: uuid::Uuid) -> bool {
    editor == Some(sender)
}

impl JavaClient {
    pub fn handle_sign_update(&self, player: &Player, sign_data: &SUpdateSign<'_>) {
        let world = player.get_entity().world.load_full();
        let Some(block_entity) = world.get_block_entity(&sign_data.location) else {
            return;
        };
        let Some(sign_entity) =
            crate::block::entities::sign::SignEntityRef::from_block_entity(&*block_entity)
        else {
            return;
        };
        if sign_entity.is_waxed() {
            return;
        }

        let currently_editing = *sign_entity
            .currently_editing_player()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !has_editor_grant(currently_editing, player.gameprofile.id) {
            return;
        }

        let lines = vec![
            sign_data.line_1.to_string(),
            sign_data.line_2.to_string(),
            sign_data.line_3.to_string(),
            sign_data.line_4.to_string(),
        ];

        if let Some(player_arc) = world.get_player_by_uuid(player.gameprofile.id) {
            let mut event = crate::plugin::api::events::block::sign_change::SignChangeEvent::new(
                player_arc,
                sign_data.location,
                lines,
            );
            if let Some(server) = world.server.upgrade() {
                server.plugin_manager.fire_blocking(&server, &mut event);
            }
            if event.cancelled {
                return;
            }
        }

        let mut editor = sign_entity
            .currently_editing_player()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !has_editor_grant(*editor, player.gameprofile.id) {
            return;
        }

        let text = sign_entity.get_text(sign_data.is_front_text);

        *text
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = [
            sign_data.line_1.into(),
            sign_data.line_2.into(),
            sign_data.line_3.into(),
            sign_data.line_4.into(),
        ];
        *editor = None;
        drop(editor);
        world.update_block_entity(&block_entity);
    }
}

#[cfg(test)]
mod tests {
    use super::has_editor_grant;
    use uuid::Uuid;

    #[test]
    fn editor_grant_must_match_sender() {
        let sender = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);

        assert!(!has_editor_grant(None, sender));
        assert!(!has_editor_grant(Some(other), sender));
        assert!(has_editor_grant(Some(sender), sender));
    }
}
