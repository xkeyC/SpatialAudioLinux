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

    /// Spherical interpolation, `t` in 0..=1.
    pub fn slerp(self, mut o: Quat, t: f32) -> Quat {
        let mut dot = self.w * o.w + self.x * o.x + self.y * o.y + self.z * o.z;
        if dot < 0.0 {
            o = Quat {
                w: -o.w,
                x: -o.x,
                y: -o.y,
                z: -o.z,
            };
            dot = -dot;
        }
        if dot > 0.9995 {
            return Quat {
                w: self.w + (o.w - self.w) * t,
                x: self.x + (o.x - self.x) * t,
                y: self.y + (o.y - self.y) * t,
                z: self.z + (o.z - self.z) * t,
            }
            .normalize();
        }
        let theta = dot.clamp(-1.0, 1.0).acos();
        let s = theta.sin();
        let a = ((1.0 - t) * theta).sin() / s;
        let b = (t * theta).sin() / s;
        Quat {
            w: self.w * a + o.w * b,
            x: self.x * a + o.x * b,
            y: self.y * a + o.y * b,
            z: self.z * a + o.z * b,
        }
        .normalize()
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

/// Tracks the head orientation relative to the "front" chosen at recentring.
#[derive(Clone, Debug)]
pub struct HeadPose {
    reference: Option<Quat>,
    current: Quat,
    /// Flip the interpretation of the tracker quaternion (world->head instead
    /// of head->world), for trackers that report the inverse rotation.
    invert: bool,
}

impl HeadPose {
    pub fn new(invert: bool) -> Self {
        HeadPose {
            reference: None,
            current: Quat::IDENTITY,
            invert,
        }
    }

    pub fn update(&mut self, q: Quat) {
        self.current = q;
        if self.reference.is_none() {
            self.reference = Some(q);
        }
    }

    pub fn recenter(&mut self) {
        self.reference = Some(self.current);
    }

    pub fn reset(&mut self) {
        self.reference = None;
        self.current = Quat::IDENTITY;
    }

    /// Slowly pull the reference towards the current orientation.
    pub fn drift_towards_current(&mut self, t: f32) {
        if let Some(r) = self.reference {
            self.reference = Some(r.slerp(self.current, t));
        }
    }

    /// Rotation that maps a direction fixed in the room (expressed in the
    /// head frame at recentring) into the current head frame.
    pub fn room_to_head(&self) -> Quat {
        let r = self.reference.unwrap_or(Quat::IDENTITY);
        let q = self.current;
        if self.invert {
            q.mul(r.conj())
        } else {
            q.conj().mul(r)
        }
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
    fn recenter_resets_front() {
        let mut pose = HeadPose::new(false);
        pose.update(Quat::from_rotation_vector([0.0, 0.0, 0.5]));
        pose.update(Quat::from_rotation_vector([0.0, 0.0, 1.0]));
        pose.recenter();
        let (az, _) = pose.front_source_angles();
        assert!(az.abs() < 1e-3);
    }
}
