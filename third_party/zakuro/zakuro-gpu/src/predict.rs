//! Responsive movement, the GPU's side: the pictures in between drawn a little ahead of the title's own, the player
//! one way and everything else another, from transforms its host works out (RE:DEX: the local trainer predicted from
//! live input). Render side only: only the draws in between change, never the title's own draw or its memory.
//! - **Which draws.** Those whose vertex shader places them w/ the field camera's projection: four float uniforms in a
//!   row, among the ones that place its vertices, that look like the perspective `g` was made w/ (its rows' lengths,
//!   and w = -z). A 2D draw, or one w/ another camera, is left as frame interpolation has it.
//! - **How.** A clip-space transform folded into those four rows: the shader dots them w/ a point whose w is 1, so
//!   `C * rows` places each vertex at `C * clip`. One transform per picture in between for the player's draws, one for
//!   the rest (the camera's own move).
//! - **The player's draws, by what they are.** While the host says it's calm (the player stands, nobody near), each
//!   draw's mesh (its vertex arrays, see interpolation::mesh) is a candidate when every draw of it in the frame has all
//!   its sampled vertices (a few, shaded on the CPU and taken back to the world thru `g`) in a cylinder on the feet:
//!   a floor, grass or a puddle drawn elsewhere too never is. It's learned once it was at the feet standing at two
//!   places a tile or more apart, which a door mat or a warp panel under a standing player never is (it stays put).
//!   Learning stops at a place once nothing new came for a while. Another look starts it over; on another map what
//!   was learned is kept for what is wholly at the feet in the first calm frame there. Ahead, a draw is the player's when its mesh
//!   was learned and it's still mostly at the feet, or it's the shadow: small, flat and on the floor right under them.
//!   Nothing is drawn ahead before the player's meshes are known.

use std::collections::{HashMap, HashSet};

use crate::shader::Vec4;
use crate::studio::{self, M4};

/// what the pictures in between of the next picture show ahead of frame interpolation
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prediction {
    /// world to the GPU's clip space as the picture is drawn (the 3DS's sideways turn times the field camera)
    pub g: M4,
    /// the player's feet as the picture draws them, and the cylinder that holds the player, world units
    pub feet: [f32; 3],
    pub radius: f32,
    pub height: f32,
    /// the map, and the player's look (outfit, form): the player's meshes are learned again on another
    pub map: u16,
    pub look: u64,
    /// the player stands and nobody is near: its draws are learned (see Trainer)
    pub learn: bool,
    /// per picture in between k (from 0): the transforms for the player's draws and for the rest, in clip space
    pub player: Vec<M4>,
    pub world: Vec<M4>,
}

pub(crate) const IDENTITY: M4 = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];

/// how many of a draw's vertices tell whose it is
pub(crate) const SAMPLES: usize = 8;
/// the shadow under the feet: within this of them across, a unit of them up or down, and flat
const SHADOW: f32 = 8.0;

impl Prediction {
    /// does it move anything
    pub fn moves(&self) -> bool {
        self.player.iter().chain(&self.world).any(|c| *c != IDENTITY)
    }

    /// the first of the four uniforms in a row among `placing` (one bit each) that hold the field camera's projection,
    /// none when the draw has none
    pub(crate) fn projection_rows(&self, uniforms: &[Vec4], placing: u128) -> Option<usize> {
        let len = |r: &[f32]| (r[0] * r[0] + r[1] * r[1] + r[2] * r[2]).sqrt();
        let (x, y) = (len(&self.g[0]), len(&self.g[1]));
        let near = |a: f32, b: f32| (a - b).abs() <= 1e-3 * b.abs().max(1.0);
        (0..uniforms.len().saturating_sub(3)).find(|&i| {
            placing >> i & 0xF == 0xF
                && uniforms[i + 3] == [0.0, 0.0, -1.0, 0.0]
                && near(len(&uniforms[i]), x)
                && near(len(&uniforms[i + 1]), y)
                && uniforms[i][3] == 0.0
                && uniforms[i + 1][3] == 0.0
        })
    }

    /// clip positions (the picture's own clip space) back to the world; ones at w = 0 left out
    pub(crate) fn world(gi: &M4, clips: &[[f32; 4]]) -> Vec<[f32; 3]> {
        clips
            .iter()
            .filter_map(|c| {
                let p = studio::mul_v(gi, c);
                (p[3].abs() > 1e-9).then(|| [p[0] / p[3], p[1] / p[3], p[2] / p[3]])
            })
            .collect()
    }

    /// how many of the points are in the cylinder on the feet
    fn at_feet(&self, points: &[[f32; 3]]) -> usize {
        let f = self.feet;
        points.iter().filter(|p| (p[0] - f[0]).hypot(p[2] - f[2]) <= self.radius && (-2.0..=self.height).contains(&(p[1] - f[1]))).count()
    }

    /// all of them in the cylinder on the feet (a draw learned as the player's, see Trainer)
    pub(crate) fn wholly_at_feet(&self, points: &[[f32; 3]]) -> bool {
        !points.is_empty() && self.at_feet(points) == points.len()
    }

    /// the player's shadow: small, flat and on the floor right under the feet (a floor, a mat or a neighbour's shadow
    /// reaches past it)
    pub(crate) fn shadow(&self, points: &[[f32; 3]]) -> bool {
        let f = self.feet;
        let (lo, hi) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        !points.is_empty() && hi - lo < 1.0 && points.iter().all(|p| (p[0] - f[0]).hypot(p[2] - f[2]) <= SHADOW && (p[1] - f[1]).abs() <= 1.0)
    }
}

/// the player's draws, learned (see the module's notes)
#[derive(Default)]
pub(crate) struct Trainer {
    /// the map and look learned for
    at: Option<(u16, u64)>,
    meshes: HashSet<u64>,
    /// candidates: where the player stood when each was first wholly at its feet
    first: HashMap<u64, [f32; 3]>,
    /// this frame's draws by mesh while learning: all of them wholly at the feet; and the feet then
    seen: HashMap<u64, bool>,
    feet: Option<[f32; 3]>,
    /// learning frames in a row that learned nothing, and where the player stood
    /// when that began: learning there stops after QUIET of them, a tile away it starts again
    quiet: u32,
    quiet_at: Option<[f32; 3]>,
    /// what was learned on the map b4, same look: each is learned again seen wholly at the feet in the first calm frame
    /// here (a map's ids change on a warp or a zone line, the player's model mostly doesn't)
    carried: HashSet<u64>,
}

/// the most meshes kept (X's trainer is a dozen or two draws)
const MESHES: usize = 128;
/// a candidate is learned seen at the feet standing this far from where it first was (a tile)
const APART: f32 = 18.0;
/// learning frames w/ nothing new after which learning stops where the player stands (till it stands a tile away, or
/// another map or look)
const QUIET: u32 = 30;

impl Trainer {
    /// the map or the look changed: what was learned is no longer the player's for sure (another look: not at all)
    fn follow(&mut self, prediction: &Prediction) {
        if self.at != Some((prediction.map, prediction.look)) {
            let same_look = self.at.is_some_and(|(_, look)| look == prediction.look);
            let carried = if same_look { self.meshes.drain().chain(self.carried.drain()).collect() } else { HashSet::new() };
            *self = Trainer { at: Some((prediction.map, prediction.look)), carried, ..Trainer::default() };
        }
    }

    /// does a draw need its vertices shaded to learn from it
    pub(crate) fn learning(&mut self, prediction: &Prediction) -> bool {
        self.follow(prediction);
        let elsewhere = self.quiet_at.is_none_or(|at| (at[0] - prediction.feet[0]).hypot(at[2] - prediction.feet[2]) >= APART);
        prediction.learn && (self.quiet < QUIET || elsewhere)
    }

    /// a draw w/ the field camera's projection: noted while learning; whether it's the player's
    pub(crate) fn draw(&mut self, prediction: &Prediction, mesh: u64, points: &[[f32; 3]]) -> bool {
        if self.learning(prediction) {
            *self.seen.entry(mesh).or_insert(true) &= prediction.wholly_at_feet(points);
            self.feet = Some(prediction.feet);
        }
        let known = self.meshes.contains(&mesh) && 2 * prediction.at_feet(points) > points.len();
        known || prediction.shadow(points)
    }

    /// does it know the player's draws yet
    pub(crate) fn knows(&self) -> bool {
        !self.meshes.is_empty()
    }

    /// the title's frame is done: a candidate seen at the feet a tile from where it first was is learned
    pub(crate) fn frame_done(&mut self) {
        let Some(feet) = self.feet.take() else {
            self.seen.clear();
            return;
        };
        let before = self.meshes.len();
        for (mesh, all) in self.seen.drain() {
            if !all || self.meshes.contains(&mesh) {
                continue;
            }
            if self.carried.contains(&mesh) {
                self.meshes.insert(mesh);
                continue;
            }
            match self.first.get(&mesh) {
                Some(first) if (first[0] - feet[0]).hypot(first[2] - feet[2]) >= APART => {
                    self.meshes.insert(mesh);
                }
                Some(_) => {}
                None => {
                    self.first.insert(mesh, feet);
                }
            }
        }
        self.carried.clear();
        let far = self.quiet_at.is_none_or(|at| (at[0] - feet[0]).hypot(at[2] - feet[2]) >= APART);
        // (new candidates don't count: meshes drawn from new arrays each frame would keep it learning forever)
        if self.meshes.len() != before || far {
            (self.quiet, self.quiet_at) = (0, Some(feet));
        } else {
            self.quiet += 1;
        }
        // candidates that never came along pile up (a mesh's arrays may be new each frame): they go, what's learned stays
        if self.first.len() > 16 * MESHES {
            self.first.clear();
        }
        if self.meshes.len() > MESHES {
            self.meshes.clear();
        }
    }
}

/// rows `at..at + 4` of the uniforms turned by clip-space transform `c`
pub(crate) fn fold(uniforms: &mut [Vec4], at: usize, c: &M4) {
    let rows: [Vec4; 4] = std::array::from_fn(|i| uniforms[at + i]);
    for (i, out) in uniforms[at..at + 4].iter_mut().enumerate() {
        *out = std::array::from_fn(|j| (0..4).map(|k| c[i][k] * rows[k][j]).sum());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the field's perspective as X gives it the GPU, sideways
    fn projection() -> [Vec4; 4] {
        [[0.0, 3.732, 0.0, 0.0], [-2.2392, 0.0, 0.0, 0.0], [0.0, 0.0, -1.01, -20.0], [0.0, 0.0, -1.0, 0.0]]
    }

    fn prediction() -> Prediction {
        let view: M4 = [[1.0, 0.0, 0.0, -100.0], [0.0, 0.6, -0.8, 10.0], [0.0, 0.8, 0.6, -300.0], [0.0, 0.0, 0.0, 1.0]];
        let p = projection();
        Prediction { g: studio::mul(&p, &view), feet: [100.0, 0.0, 0.0], radius: 14.0, height: 48.0, map: 200, look: 1, learn: true, player: vec![IDENTITY], world: vec![IDENTITY] }
    }

    #[test]
    fn the_field_projection_is_found_among_the_placing_uniforms() {
        let pr = prediction();
        let mut u = vec![[0.0; 4]; 96];
        u[86..90].copy_from_slice(&projection());
        assert_eq!(pr.projection_rows(&u, 0xF << 86), Some(86));
        assert_eq!(pr.projection_rows(&u, 0x7 << 86), None, "rows the shader doesn't read");
        // an orthographic 2D camera
        u[86..90].copy_from_slice(&[[0.0, 0.0083, 0.0, 0.0], [-0.0063, 0.0, 0.0, 0.0], [0.0, 0.0, 0.001, 0.0], [0.0, 0.0, 0.0, 1.0]]);
        assert_eq!(pr.projection_rows(&u, 0xF << 86), None);
        assert!(!pr.moves());
    }

    #[test]
    fn a_folded_transform_moves_the_clip_positions() {
        let p = projection();
        let mut u = vec![[0.0; 4]; 96];
        u[10..14].copy_from_slice(&p);
        let c: M4 = [[1.0, 0.0, 0.0, 0.5], [0.0, 2.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.1, 1.0]];
        fold(&mut u, 10, &c);
        let v = [3.0, -2.0, -50.0, 1.0];
        let dot = |r: &Vec4| (0..4).map(|i| r[i] * v[i]).sum::<f32>();
        let clip: [f32; 4] = std::array::from_fn(|i| dot(&p[i]));
        let moved: [f32; 4] = std::array::from_fn(|i| (0..4).map(|k| c[i][k] * clip[k]).sum());
        for i in 0..4 {
            assert!((dot(&u[10 + i]) - moved[i]).abs() < 1e-3, "{i}");
        }
    }

    #[test]
    fn points_go_back_to_the_world() {
        let pr = prediction();
        let gi = studio::invert(&pr.g).unwrap();
        let p = [104.0, 30.0, 2.0];
        let back = Prediction::world(&gi, &[studio::mul_v(&pr.g, &[p[0], p[1], p[2], 1.0])]);
        assert!((0..3).all(|i| (back[0][i] - p[i]).abs() < 1e-2), "{back:?}");
    }

    #[test]
    fn the_shadow_is_small_flat_and_right_under_the_feet() {
        let pr = prediction();
        assert!(pr.shadow(&[[96.0, 0.2, 3.0], [104.0, 0.2, -3.0]]));
        // a floor quad w/ a corner at the feet, a door mat on the player's tile, a neighbour's shadow a tile away
        assert!(!pr.shadow(&[[100.0, 0.0, 0.0], [190.0, 0.0, 0.0], [100.0, 0.0, 90.0], [190.0, 0.0, 90.0]]));
        assert!(!pr.shadow(&[[91.0, 0.0, -9.0], [109.0, 0.0, -9.0], [91.0, 0.0, 9.0], [109.0, 0.0, 9.0]]));
        assert!(!pr.shadow(&[[114.0, 0.2, 3.0], [122.0, 0.2, -3.0]]));
        // flat but up high (a hat's band) is no shadow
        assert!(!pr.shadow(&[[98.0, 40.0, 1.0], [102.0, 40.0, -1.0]]));
        assert!(!pr.shadow(&[]));
    }

    /// a calm frame w/ the player standing at x: its body (mesh 1), and whatever else is drawn
    fn calm_frame(t: &mut Trainer, pr: &mut Prediction, x: f32, others: &[(u64, Vec<[f32; 3]>)]) {
        pr.feet = [x, 0.0, 0.0];
        pr.learn = true;
        t.draw(pr, 1, &[[x, 0.0, 0.0], [x + 4.0, 30.0, 2.0], [x - 3.0, 40.0, -3.0]]);
        for (mesh, points) in others {
            t.draw(pr, *mesh, points);
        }
        t.frame_done();
    }

    #[test]
    fn the_players_meshes_are_learned_standing_a_tile_apart_and_never_from_elsewhere() {
        let mut pr = prediction();
        let mut t = Trainer::default();
        // grass drawn under the feet and elsewhere from one mesh (2); a door mat under the player's first place (3)
        let grass = |x: f32| vec![(2, vec![[x - 5.0, 0.0, -5.0], [x + 5.0, 6.0, 5.0]]), (2, vec![[160.0, 0.0, 40.0], [170.0, 6.0, 50.0]])];
        let mat = (3, vec![[91.0, 0.0, -9.0], [109.0, 0.0, -9.0], [91.0, 0.0, 9.0], [109.0, 0.0, 9.0]]);
        let mut first = grass(100.0);
        first.push(mat.clone());
        calm_frame(&mut t, &mut pr, 100.0, &first);
        calm_frame(&mut t, &mut pr, 100.0, &first);
        assert!(!t.knows(), "one place isn't enough: a mat under the feet stays put");
        // standing a tile away: the body came along, the mat didn't
        calm_frame(&mut t, &mut pr, 118.0, &grass(118.0));
        assert!(t.knows());
        pr.learn = false;
        pr.feet = [100.0, 0.0, 0.0];
        assert!(!t.draw(&pr, 3, &mat.1), "the door mat is never the player's");
        assert!(!t.draw(&pr, 2, &grass(100.0)[0].1), "the grass mesh is drawn elsewhere too: never the player's");
        let body = [[100.0, 0.0, 0.0], [104.0, 30.0, 2.0], [97.0, 40.0, -3.0]];
        assert!(t.draw(&pr, 1, &body), "the body is the player's");
        assert!(!t.draw(&pr, 4, &body), "an unknown mesh at the feet (a puppet on the same tile) is not");
        assert!(!t.draw(&pr, 1, &[[160.0, 0.0, 40.0], [162.0, 30.0, 41.0]]), "the body's mesh away from the feet is not");
        // another map (a warp, a zone line): what's wholly at the feet in the first calm frame there is known again,
        // what isn't (a mesh learned that isn't the player's after all) is gone
        let mut m = Trainer::default();
        calm_frame(&mut m, &mut pr, 100.0, &[(5, vec![[100.0, 0.0, 0.0]])]);
        calm_frame(&mut m, &mut pr, 118.0, &[(5, vec![[118.0, 0.0, 0.0]])]);
        pr.map = 201;
        calm_frame(&mut m, &mut pr, 300.0, &[(5, vec![[160.0, 0.0, 40.0]])]);
        pr.learn = false;
        assert!(m.draw(&pr, 1, &[[300.0, 0.0, 0.0], [304.0, 30.0, 2.0]]), "the body carried over");
        assert!(!m.draw(&pr, 5, &[[300.0, 0.0, 0.0], [302.0, 30.0, 1.0]]), "a mesh not at the feet there went");
        pr.map = 200;
        // another look (an outfit, the bike) learns again
        pr.look = 2;
        assert!(!t.draw(&pr, 1, &body));
        assert!(!t.knows());
    }

    #[test]
    fn learning_stops_once_nothing_new_comes() {
        let mut pr = prediction();
        let mut t = Trainer::default();
        // standing long at the first place stops learning there; a tile away it starts again and teaches
        for _ in 0..2 * QUIET {
            calm_frame(&mut t, &mut pr, 100.0, &[]);
        }
        assert!(!t.knows() && !t.learning(&pr), "no shading every frame while standing");
        pr.feet = [118.0, 0.0, 0.0];
        assert!(t.learning(&pr));
        calm_frame(&mut t, &mut pr, 118.0, &[]);
        assert!(t.knows() && t.learning(&pr));
        for _ in 0..QUIET {
            calm_frame(&mut t, &mut pr, 118.0, &[]);
        }
        assert!(!t.learning(&pr), "no more shading to learn");
        // a mesh at the feet from new arrays every frame neither keeps it learning nor, piling up, wipes what's known
        let mut u = Trainer::default();
        calm_frame(&mut u, &mut pr, 100.0, &[]);
        for k in 0..20 * MESHES as u64 {
            let x = if k == 0 { 118.0 } else { 118.0 + (k % 2) as f32 };
            calm_frame(&mut u, &mut pr, x, &[(1000 + k, vec![[x, 0.0, 0.0]])]);
        }
        assert!(u.knows() && !u.learning(&pr));
        pr.map = 9;
        assert!(t.learning(&pr), "a new map learns again");
    }
}
