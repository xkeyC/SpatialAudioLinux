//! Minimal quaternion math and the head pose relative to the recentred front.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat {
    pub w: f32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Quat {
    pub const IDENTITY: Quat = Quat {
        w: 1.0,
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// Rotation of `|v|` radians about the axis `v / |v|`.
    pub fn from_rotation_vector(v: [f32; 3]) -> Quat {
        let angle = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if angle < 1e-9 {
            return Quat::IDENTITY;
        }
        let s = (angle * 0.5).sin() / angle;
        Quat {
            w: (angle * 0.5).cos(),
            x: v[0] * s,
            y: v[1] * s,
            z: v[2] * s,
        }
    }

    pub fn conj(self) -> Quat {
        Quat {
            w: self.w,
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
    }

    pub fn mul(self, o: Quat) -> Quat {
        Quat {
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
        }
    }

    pub fn normalize(self) -> Quat {
        let n = (self.w * self.w + self.x * self.x + self.y * self.y + self.z * self.z).sqrt();
        if n < 1e-9 {
            return Quat::IDENTITY;
        }
        Quat {
            w: self.w / n,
            x: self.x / n,
            y: self.y / n,
            z: self.z / n,
        }
    }

    pub fn rotate(self, v: [f32; 3]) -> [f32; 3] {
        let p = Quat {
            w: 0.0,
            x: v[0],
            y: v[1],
            z: v[2],
        };
        let r = self.mul(p).mul(self.conj());
        [r.x, r.y, r.z]
    }
}

/// Convert a vector from the Android head frame (+X right ear, +Y nose,
/// +Z up) to the SOFA listener frame (+X front, +Y left, +Z up).
pub fn head_to_sofa(v: [f32; 3]) -> [f32; 3] {
    [v[1], -v[0], v[2]]
}

/// Convert a SOFA direction (+X front, +Y left, +Z up) to the head frame.
pub fn sofa_to_head(v: [f32; 3]) -> [f32; 3] {
    [-v[1], v[0], v[2]]
}

/// Unit direction in the SOFA frame for an azimuth (degrees, counter-clockwise
/// from the front, i.e. left is positive) and elevation (degrees, up positive).
pub fn sofa_direction(azimuth_deg: f32, elevation_deg: f32) -> [f32; 3] {
    let (az, el) = (azimuth_deg.to_radians(), elevation_deg.to_radians());
    [el.cos() * az.cos(), el.cos() * az.sin(), el.sin()]
}

/// Heading (rotation about the reference +Z axis) of a forward direction
/// given in the tracker reference frame.
fn heading_of(forward: [f32; 3], up: [f32; 3]) -> f32 {
    let (fx, fy) = if forward[0].hypot(forward[1]) > 0.2 {
        (forward[0], forward[1])
    } else {
        // Looking almost straight up/down: the top of the head points
        // backwards/forwards instead.
        let s = if forward[2] > 0.0 { -1.0 } else { 1.0 };
        (up[0] * s, up[1] * s)
    };
    (-fx).atan2(fy)
}

fn wrap_angle(a: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    (a + PI).rem_euclid(TAU) - PI
}

/// Tracks the head orientation relative to the "front" chosen at recentring.
///
/// The tracker reports the orientation of *its* frame relative to a
/// gravity-aligned reference (+Z up). Earbuds sit tilted in the ear, so their
/// frame is off from the real head frame by a fixed rotation. Recentring
/// assumes the user looks straight ahead with a level head: it keeps only the
/// heading as the room's front and stores the remaining tilt as a
/// mount calibration.
///
/// Like Android's `HeadTrackingProcessor`, changes of the front are applied
/// gradually (see [`HeadPose::advance`]) so recentring never jumps.
#[derive(Clone, Debug)]
pub struct HeadPose {
    /// Heading of the room's front in the reference frame (radians), as
    /// currently rendered.
    front_heading: Option<f32>,
    /// Heading the front is moving towards.
    target_heading: f32,
    /// Mount calibration: tracker frame -> real head frame.
    calibration: Quat,
    current: Quat,
    /// Flip the interpretation of the tracker quaternion (world->head instead
    /// of head->world), for trackers that report the inverse rotation.
    invert: bool,
}

impl HeadPose {
    pub fn new(invert: bool) -> Self {
        HeadPose {
            front_heading: None,
            target_heading: 0.0,
            calibration: Quat::IDENTITY,
            current: Quat::IDENTITY,
            invert,
        }
    }

    /// Tracker frame -> reference frame.
    fn tracker(&self) -> Quat {
        if self.invert {
            self.current.conj()
        } else {
            self.current
        }
    }

    /// Real head frame -> reference frame.
    pub fn head(&self) -> Quat {
        self.tracker().mul(self.calibration)
    }

    pub fn update(&mut self, q: Quat) {
        self.current = q;
    }

    pub fn is_centered(&self) -> bool {
        self.front_heading.is_some()
    }

    /// Make the current direction the front, treating the current head
    /// orientation as level (this also recalibrates the mount tilt).
    pub fn recenter(&mut self) {
        let e = self.tracker();
        let heading = heading_of(e.rotate([0.0, 1.0, 0.0]), e.rotate([0.0, 0.0, 1.0]));
        let level = Quat::from_rotation_vector([0.0, 0.0, heading]);
        self.calibration = e.conj().mul(level).normalize();
        self.target_heading = heading;
        if self.front_heading.is_none() {
            self.front_heading = Some(heading);
        }
    }

    /// Make the current heading the front, keeping the mount calibration
    /// (used for automatic recentring, where the head may not be level).
    pub fn recenter_heading(&mut self) {
        if self.front_heading.is_some() {
            self.target_heading = self.current_heading();
        }
    }

    /// Make the current heading the front immediately, keeping the mount
    /// calibration (after the tracker reset its reference frame, the old
    /// heading means nothing in the new frame).
    pub fn rebase_heading(&mut self) {
        if self.front_heading.is_some() {
            let heading = self.current_heading();
            self.target_heading = heading;
            self.front_heading = Some(heading);
        }
    }

    /// Move the rendered front towards its target by at most
    /// `max_rate * dt` radians.
    pub fn advance(&mut self, dt: f32, max_rate: f32) {
        if let Some(front) = self.front_heading {
            let delta = wrap_angle(self.target_heading - front);
            let step = max_rate * dt;
            self.front_heading = Some(if delta.abs() <= step {
                self.target_heading
            } else {
                wrap_angle(front + step.copysign(delta))
            });
        }
    }

    pub fn reset(&mut self) {
        self.front_heading = None;
        self.target_heading = 0.0;
        self.calibration = Quat::IDENTITY;
        self.current = Quat::IDENTITY;
    }

    /// Current heading of the (calibrated) head, in radians.
    fn current_heading(&self) -> f32 {
        let h = self.head();
        heading_of(h.rotate([0.0, 1.0, 0.0]), h.rotate([0.0, 0.0, 1.0]))
    }

    /// Rotation that maps a direction fixed in the room (expressed in the
    /// level head frame at recentring) into the current head frame.
    /// Identity until the first recentring.
    pub fn room_to_head(&self) -> Quat {
        let Some(front) = self.front_heading else {
            return Quat::IDENTITY;
        };
        let room = Quat::from_rotation_vector([0.0, 0.0, front]);
        self.head().conj().mul(room).normalize()
    }

    /// Where a source straight ahead in the room now appears, as
    /// (azimuth, elevation) in degrees; azimuth is positive to the left.
    pub fn front_source_angles(&self) -> (f32, f32) {
        let d = head_to_sofa(self.room_to_head().rotate(sofa_to_head([1.0, 0.0, 0.0])));
        let az = d[1].atan2(d[0]).to_degrees();
        let el = d[2].clamp(-1.0, 1.0).asin().to_degrees();
        (az, el)
    }
}

/// Rotation speed between two orientations, in rad/s.
pub fn angular_speed(a: Quat, b: Quat, dt: f32) -> f32 {
    let d = a.conj().mul(b);
    let angle = 2.0 * d.w.abs().clamp(0.0, 1.0).acos();
    if dt > 0.0 { angle / dt } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn frames_round_trip() {
        let v = [0.3, -0.2, 0.9];
        assert!(close(head_to_sofa(sofa_to_head(v)), v));
        // Nose (+Y head) is the SOFA front (+X).
        assert!(close(head_to_sofa([0.0, 1.0, 0.0]), [1.0, 0.0, 0.0]));
        // Right ear (+X head) is SOFA -Y.
        assert!(close(head_to_sofa([1.0, 0.0, 0.0]), [0.0, -1.0, 0.0]));
    }

    #[test]
    fn turning_left_moves_front_to_the_right() {
        let mut pose = HeadPose::new(false);
        pose.update(Quat::IDENTITY);
        pose.recenter();
        // Head turns 90 degrees left = +90 degrees about the up axis (+Z),
        // with the quaternion mapping head-frame vectors into the room.
        pose.update(Quat::from_rotation_vector([
            0.0,
            0.0,
            std::f32::consts::FRAC_PI_2,
        ]));
        let (az, el) = pose.front_source_angles();
        assert!((az + 90.0).abs() < 1e-3, "az = {az}");
        assert!(el.abs() < 1e-3);
    }

    #[test]
    fn tilted_mount_is_calibrated_out() {
        use std::f32::consts::FRAC_PI_2;
        // Earbud frame pitched 41 degrees relative to the head, head facing
        // an arbitrary heading of 20 degrees.
        let tilt = Quat::from_rotation_vector([41f32.to_radians(), 0.0, 0.0]);
        let heading = Quat::from_rotation_vector([0.0, 0.0, 20f32.to_radians()]);
        let mut pose = HeadPose::new(false);
        pose.update(heading.mul(tilt));
        pose.recenter();
        let (az, el) = pose.front_source_angles();
        assert!(az.abs() < 1e-3 && el.abs() < 1e-3, "az={az} el={el}");
        // Turn the head 90 degrees left about the vertical axis.
        let yaw = Quat::from_rotation_vector([0.0, 0.0, FRAC_PI_2]);
        pose.update(yaw.mul(heading).mul(tilt));
        let (az, el) = pose.front_source_angles();
        assert!((az + 90.0).abs() < 1e-2, "az={az}");
        assert!(el.abs() < 1e-2, "el={el}");
        // Look up 30 degrees (about the head's ear axis).
        let up = Quat::from_rotation_vector([30f32.to_radians(), 0.0, 0.0]);
        pose.update(heading.mul(up).mul(tilt));
        let (az, el) = pose.front_source_angles();
        assert!(az.abs() < 1e-2, "az={az}");
        assert!((el + 30.0).abs() < 1e-2, "el={el}");
    }

    #[test]
    fn auto_recenter_is_rate_limited_and_keeps_tilt() {
        let tilt = Quat::from_rotation_vector([0.7, 0.0, 0.0]);
        let mut pose = HeadPose::new(false);
        pose.update(tilt);
        pose.recenter();
        // Head turned 1 rad left and looking 0.3 rad down, held still.
        let turned = Quat::from_rotation_vector([0.0, 0.0, 1.0])
            .mul(Quat::from_rotation_vector([-0.3, 0.0, 0.0]))
            .mul(tilt);
        pose.update(turned);
        pose.recenter_heading();
        pose.advance(0.5, 0.8);
        // After 0.5 s at 0.8 rad/s the front has moved 0.4 of the 1 rad turn
        // (the apparent azimuth is a bit larger because the head is pitched).
        let (az, _) = pose.front_source_angles();
        assert!((-38.0..-34.0).contains(&az), "az={az}");
        pose.advance(10.0, 0.8);
        let (az, el) = pose.front_source_angles();
        assert!(az.abs() < 0.1, "az={az}");
        // The downward look is still a downward look: front appears above.
        assert!((el - 0.3f32.to_degrees()).abs() < 0.1, "el={el}");
    }

    #[test]
    fn recenter_resets_front() {
        let mut pose = HeadPose::new(false);
        pose.update(Quat::from_rotation_vector([0.0, 0.0, 0.5]));
        pose.update(Quat::from_rotation_vector([0.0, 0.0, 1.0]));
        pose.recenter();
        let (az, _) = pose.front_source_angles();
        assert!(az.abs() < 1e-3);
    }
}
