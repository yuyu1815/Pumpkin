#[allow(clippy::wildcard_imports)]
use super::*;

enum TeleportConfirmation {
    Confirmed(Vector3<f64>),
    Ignore,
    Duplicate,
}

fn classify_confirmation(
    awaiting_teleport: &mut Option<(VarInt, Vector3<f64>)>,
    last_teleport_id: VarInt,
    last_teleport_id_issued: bool,
    teleport_id: VarInt,
) -> TeleportConfirmation {
    if let Some((pending_id, position)) = awaiting_teleport.as_ref() {
        if *pending_id == teleport_id {
            let position = *position;
            *awaiting_teleport = None;
            return TeleportConfirmation::Confirmed(position);
        }
        return TeleportConfirmation::Ignore;
    }

    if last_teleport_id_issued && teleport_id == last_teleport_id {
        TeleportConfirmation::Duplicate
    } else {
        TeleportConfirmation::Ignore
    }
}

impl JavaClient {
    pub fn handle_confirm_teleport(&self, player: &Player, confirm_teleport: &SConfirmTeleport) {
        let confirmation = {
            let mut awaiting_teleport = player
                .awaiting_teleport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let last_teleport_id = VarInt(
                player
                    .teleport_id_count
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            classify_confirmation(
                &mut awaiting_teleport,
                last_teleport_id,
                player
                    .last_teleport_id_issued
                    .load(std::sync::atomic::Ordering::Relaxed),
                confirm_teleport.teleport_id,
            )
        };

        match confirmation {
            TeleportConfirmation::Confirmed(position) => {
                player.get_entity().set_pos(position);
                self.reset_movement_position(position);
            }
            TeleportConfirmation::Ignore => {}
            TeleportConfirmation::Duplicate => self.try_kick(&TextComponent::translate_cross(
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_PLAYER_MOVEMENT,
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_PLAYER_MOVEMENT,
                [],
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_confirmation, TeleportConfirmation};
    use pumpkin_protocol::codec::var_int::VarInt;
    use pumpkin_util::math::vector3::Vector3;

    #[test]
    fn teleport_confirmation_matches_vanilla_id_state() {
        let target = Vector3::new(12.0, 64.0, -3.0);
        let mut awaiting = Some((VarInt(9), target));

        assert!(matches!(
            classify_confirmation(&mut awaiting, VarInt(9), true, VarInt(8)),
            TeleportConfirmation::Ignore
        ));
        assert_eq!(awaiting, Some((VarInt(9), target)));
        assert!(matches!(
            classify_confirmation(&mut awaiting, VarInt(9), true, VarInt(9)),
            TeleportConfirmation::Confirmed(position) if position == target
        ));
        assert_eq!(awaiting, None);
        assert!(matches!(
            classify_confirmation(&mut awaiting, VarInt(9), true, VarInt(8)),
            TeleportConfirmation::Ignore
        ));
        assert!(matches!(
            classify_confirmation(&mut awaiting, VarInt(9), true, VarInt(9)),
            TeleportConfirmation::Duplicate
        ));
        assert!(matches!(
            classify_confirmation(&mut awaiting, VarInt(0), false, VarInt(0)),
            TeleportConfirmation::Ignore
        ));
        assert!(matches!(
            classify_confirmation(&mut awaiting, VarInt(0), true, VarInt(0)),
            TeleportConfirmation::Duplicate
        ));
        assert!(matches!(
            classify_confirmation(&mut awaiting, VarInt(1), true, VarInt(0)),
            TeleportConfirmation::Ignore
        ));
    }
}
