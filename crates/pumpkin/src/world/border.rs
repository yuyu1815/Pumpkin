use pumpkin_protocol::java::client::play::{
    CInitializeWorldBorder, CSetBorderCenter, CSetBorderLerpSize, CSetBorderSize,
    CSetBorderWarningDelay, CSetBorderWarningDistance,
};

use crate::net::java::JavaClient;

use super::World;

pub struct Worldborder {
    pub center_x: f64,
    pub center_z: f64,
    pub old_diameter: f64,
    pub new_diameter: f64,
    /// Total interpolation duration in game ticks.
    pub speed: i64,
    lerp_elapsed: i64,
    pub portal_teleport_boundary: i32,
    pub warning_blocks: i32,
    pub warning_time: i32,
    pub damage_per_block: f32,
    pub buffer: f32,
}

impl Worldborder {
    #[must_use]
    pub const fn new(
        x: f64,
        z: f64,
        diameter: f64,
        speed: i64,
        warning_blocks: i32,
        warning_time: i32,
    ) -> Self {
        Self {
            center_x: x,
            center_z: z,
            old_diameter: diameter,
            new_diameter: diameter,
            speed,
            lerp_elapsed: 0,
            portal_teleport_boundary: 29_999_984,
            warning_blocks,
            warning_time,
            damage_per_block: 0.0,
            buffer: 0.0,
        }
    }

    pub fn init_client(&self, client: &JavaClient) {
        if let Ok(data) = client.serialize_packet(&CInitializeWorldBorder::new(
            self.center_x,
            self.center_z,
            self.diameter(),
            self.new_diameter,
            (self.speed - self.lerp_elapsed).max(0).into(),
            self.portal_teleport_boundary.into(),
            self.warning_blocks.into(),
            self.warning_time.into(),
        )) {
            client.try_enqueue_packet(data);
        }
    }

    pub fn set_center(&mut self, world: &World, x: f64, z: f64) {
        self.center_x = x;
        self.center_z = z;

        world.broadcast_packet_all(&CSetBorderCenter::new(self.center_x, self.center_z));
    }

    pub fn set_diameter(&mut self, world: &World, diameter: f64, speed: Option<i64>) {
        self.old_diameter = self.diameter();
        self.new_diameter = diameter;
        self.speed = speed.unwrap_or(0).max(0);
        self.lerp_elapsed = 0;

        if self.speed > 0 {
            world.broadcast_packet_all(&CSetBorderLerpSize::new(
                self.old_diameter,
                self.new_diameter,
                self.speed.into(),
            ));
        } else {
            self.old_diameter = diameter;
            world.broadcast_packet_all(&CSetBorderSize::new(self.new_diameter));
        }
    }

    pub fn add_diameter(&mut self, world: &World, offset: f64, speed: Option<i64>) {
        self.set_diameter(world, self.diameter() + offset, speed);
    }

    #[must_use]
    pub fn diameter(&self) -> f64 {
        if self.speed <= 0 {
            return self.new_diameter;
        }

        let progress = (self.lerp_elapsed as f64 / self.speed as f64).min(1.0);
        self.old_diameter + (self.new_diameter - self.old_diameter) * progress
    }

    pub fn tick(&mut self) {
        if self.speed <= 0 {
            return;
        }
        self.lerp_elapsed += 1;
        if self.lerp_elapsed >= self.speed {
            self.old_diameter = self.new_diameter;
            self.speed = 0;
            self.lerp_elapsed = 0;
        }
    }

    pub fn set_warning_delay(&mut self, world: &World, delay: i32) {
        self.warning_time = delay;

        world.broadcast_packet_all(&CSetBorderWarningDelay::new(self.warning_time.into()));
    }

    pub fn set_warning_distance(&mut self, world: &World, distance: i32) {
        self.warning_blocks = distance;

        world.broadcast_packet_all(&CSetBorderWarningDistance::new(self.warning_blocks.into()));
    }

    pub const fn set_damage_buffer(&mut self, buffer: f32) {
        self.buffer = buffer;
    }

    pub const fn set_damage_per_block(&mut self, damage: f32) {
        self.damage_per_block = damage;
    }

    pub fn reset(&mut self, world: &World) {
        self.center_x = 0.0;
        self.center_z = 0.0;
        self.old_diameter = 29_999_984.0;
        self.new_diameter = 29_999_984.0;
        self.speed = 0;
        self.lerp_elapsed = 0;
        self.portal_teleport_boundary = 29_999_984;
        self.warning_blocks = 5;
        self.warning_time = 15;
        self.damage_per_block = 0.2;
        self.buffer = 5.0;

        world.broadcast_packet_all(&CInitializeWorldBorder::new(
            self.center_x,
            self.center_z,
            self.old_diameter,
            self.new_diameter,
            self.speed.into(),
            self.portal_teleport_boundary.into(),
            self.warning_blocks.into(),
            self.warning_time.into(),
        ));
    }

    #[must_use]
    pub fn contains(&self, x: f64, z: f64) -> bool {
        let half = self.diameter() / 2.0;
        let min_x = self.center_x - half;
        let max_x = self.center_x + half;
        let min_z = self.center_z - half;
        let max_z = self.center_z + half;
        x >= min_x && x < max_x && z >= min_z && z < max_z
    }

    #[must_use]
    pub fn contains_block(&self, x: i32, z: i32) -> bool {
        self.contains(f64::from(x), f64::from(z))
            && self.contains(f64::from(x + 1), f64::from(z + 1))
    }

    #[must_use]
    pub fn clamp_block(&self, x: i32, z: i32) -> (i32, i32) {
        let half = self.diameter() / 2.0;
        // A border narrower than one block spans no block boundary, leaving `max`
        // below `min`. `Ord::clamp` panics on an inverted range, so collapse the
        // range onto the single block that holds the centre instead.
        let min_x = (self.center_x - half).floor() as i32;
        let max_x = ((self.center_x + half).floor() as i32 - 1).max(min_x);
        let min_z = (self.center_z - half).floor() as i32;
        let max_z = ((self.center_z + half).floor() as i32 - 1).max(min_z);
        (x.clamp(min_x, max_x), z.clamp(min_z, max_z))
    }
}

#[cfg(test)]
mod tests {
    use super::Worldborder;

    fn centered_border(diameter: f64) -> Worldborder {
        Worldborder::new(0.0, 0.0, diameter, 0, 5, 300)
    }

    /// `find_safe_location` only falls through to the `clamp_block` fallback
    /// because a zero-width border rejects every candidate it scans: `contains`
    /// reduces to `x >= center && x < center`, which no coordinate satisfies.
    #[test]
    fn a_zero_width_border_contains_no_block() {
        let border = centered_border(0.0);

        assert!(!border.contains_block(0, 0));
        assert!(
            (-32..=32).all(|x| (-32..=32).all(|z| !border.contains_block(x, z))),
            "a zero-width border should contain no block in the portal search area"
        );
    }

    /// `/worldborder set 0` is accepted, because the command's consumer is
    /// `BoundedNumArgumentConsumer::new().min(0.0)` and that bound is inclusive.
    #[test]
    fn clamp_block_handles_a_zero_width_border() {
        let border = centered_border(0.0);

        assert_eq!(border.clamp_block(0, 0), (0, 0));
        assert_eq!(border.clamp_block(100, -100), (0, 0));
    }

    /// A centre off the block grid puts both edges inside the same block, so the
    /// border spans no block boundary even though its diameter is not zero.
    #[test]
    fn clamp_block_handles_a_border_narrower_than_one_block() {
        let border = Worldborder::new(0.5, 0.5, 0.5, 0, 5, 300);

        assert_eq!(border.clamp_block(100, -100), (0, 0));
    }

    #[test]
    fn diameter_interpolates_over_game_ticks() {
        let mut border = centered_border(10.0);
        border.old_diameter = 10.0;
        border.new_diameter = 20.0;
        border.speed = 200;

        assert_eq!(border.diameter(), 10.0);
        assert!(!border.contains(7.0, 0.0));
        for _ in 0..100 {
            border.tick();
        }
        assert_eq!(border.diameter(), 15.0);
        assert!(border.contains(7.0, 0.0));
        for _ in 0..100 {
            border.tick();
        }
        assert_eq!(border.diameter(), 20.0);
        assert_eq!(border.speed, 0);
    }

    #[test]
    fn clamp_block_still_clamps_a_normal_border() {
        let border = centered_border(10.0);

        assert_eq!(border.clamp_block(100, 100), (4, 4));
        assert_eq!(border.clamp_block(-100, -100), (-5, -5));
        assert_eq!(border.clamp_block(2, -3), (2, -3));
    }
}
