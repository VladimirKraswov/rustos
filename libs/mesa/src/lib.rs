//! Первый рабочий срез порта Mesa для RustOS.
//!
//! В терминах Mesa этот crate содержит platform/winsys boundary и маленький
//! Gallium-like state tracker. Он намеренно не притворяется всей upstream
//! Mesa: GLSL/NIR, полный OpenGL state machine и C ABI будут подключаться за
//! тем же API после появления libc/pthread/dlopen. Уже сейчас все pixels
//! растеризует host VirGL renderer, а ring-3 `renderd` только формирует mesh и
//! bounded command stream.

#![no_std]

use rustos_virgl::{encode_mesh, encode_mesh_update, EncodeError, Vertex};

/// Версия RustOS Mesa platform ABI.
pub const PLATFORM_ABI_VERSION: u16 = 1;
/// Число кадров одного обычного запуска демонстрации.
pub const DEFAULT_DEMO_FRAMES: u32 = 180;
/// Максимум ограничивает время монопольного полноэкранного scanout.
pub const MAX_DEMO_FRAMES: u32 = 600;

/// Набор ресурсов, который renderd получил от capability-aware winsys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirglWinsysSurface {
    /// Physical framebuffer width.
    pub width: u32,
    /// Physical framebuffer height.
    pub height: u32,
    /// Imported render-target resource id.
    pub color_resource: u32,
    /// Context-local vertex-buffer resource id.
    pub vertex_resource: u32,
}

/// Выбранный профиль graphics API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiProfile {
    /// Современный programmable pipeline без compatibility state.
    OpenGlCore,
}

/// Минимальный Mesa context. Он не содержит raw MMIO pointers и может жить
/// только внутри изолированного renderd.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Context {
    surface: VirglWinsysSurface,
    profile: ApiProfile,
    pipeline_initialized: bool,
}

impl Context {
    /// Создаёт context поверх уже проверенных winsys resources.
    pub fn new(surface: VirglWinsysSurface, profile: ApiProfile) -> Result<Self, MesaError> {
        if surface.width == 0
            || surface.height == 0
            || surface.width > 16_384
            || surface.height > 16_384
            || surface.color_resource == 0
            || surface.vertex_resource == 0
        {
            return Err(MesaError::InvalidSurface);
        }
        Ok(Self {
            surface,
            profile,
            pipeline_initialized: false,
        })
    }

    /// Кодирует кадр Aurora 3D: градиентный stage и вращающийся освещённый
    /// кристалл. Projection и state tracking выполняются в userspace, но
    /// triangle setup, interpolation, shader и rasterization — только GPU.
    pub fn render_aurora_frame(
        &mut self,
        commands: &mut [u32],
        frame_index: u32,
    ) -> Result<usize, MesaError> {
        let mut vertices = [Vertex::new([0.0; 4], [0.0; 4]); SHOWCASE_VERTEX_COUNT];
        build_showcase_mesh(
            &mut vertices,
            frame_index,
            self.surface.width,
            self.surface.height,
        );
        let pulse = 0.006 * (1.0 + sin_turns(frame_index as f32 / 120.0));
        let clear = [0.010 + pulse, 0.016, 0.046 + pulse * 2.0, 1.0];
        let result = if self.pipeline_initialized {
            encode_mesh_update(
                commands,
                self.surface.width,
                self.surface.height,
                self.surface.color_resource,
                self.surface.vertex_resource,
                &vertices,
                clear,
            )
        } else {
            encode_mesh(
                commands,
                self.surface.width,
                self.surface.height,
                self.surface.color_resource,
                self.surface.vertex_resource,
                &vertices,
                clear,
            )
        };
        let words = result.map_err(MesaError::Transport)?;
        self.pipeline_initialized = true;
        Ok(words)
    }

    /// Возвращает профиль без раскрытия внутреннего winsys handle.
    pub const fn profile(&self) -> ApiProfile {
        self.profile
    }
}

/// Ошибка platform/state-tracker boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MesaError {
    /// Некорректная surface или нулевой resource id.
    InvalidSurface,
    /// VirGL transport не смог сериализовать bounded stream.
    Transport(EncodeError),
}

const SHOWCASE_VERTEX_COUNT: usize = 42;

#[derive(Clone, Copy)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

impl Vec3 {
    const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    fn rotate(self, sy: f32, cy: f32, sx: f32, cx: f32) -> Self {
        let x = self.x * cy + self.z * sy;
        let z = -self.x * sy + self.z * cy;
        Self {
            x,
            y: self.y * cx - z * sx,
            z: self.y * sx + z * cx,
        }
    }
}

#[derive(Clone, Copy)]
struct Face {
    corners: [usize; 4],
    normal: Vec3,
    color: [f32; 3],
}

const CUBE: [Vec3; 8] = [
    Vec3::new(-1.0, -1.0, -1.0),
    Vec3::new(1.0, -1.0, -1.0),
    Vec3::new(1.0, 1.0, -1.0),
    Vec3::new(-1.0, 1.0, -1.0),
    Vec3::new(-1.0, -1.0, 1.0),
    Vec3::new(1.0, -1.0, 1.0),
    Vec3::new(1.0, 1.0, 1.0),
    Vec3::new(-1.0, 1.0, 1.0),
];

const FACES: [Face; 6] = [
    Face {
        corners: [0, 1, 2, 3],
        normal: Vec3::new(0.0, 0.0, -1.0),
        color: [0.18, 0.34, 1.00],
    },
    Face {
        corners: [5, 4, 7, 6],
        normal: Vec3::new(0.0, 0.0, 1.0),
        color: [0.16, 0.94, 0.88],
    },
    Face {
        corners: [4, 0, 3, 7],
        normal: Vec3::new(-1.0, 0.0, 0.0),
        color: [0.56, 0.26, 1.00],
    },
    Face {
        corners: [1, 5, 6, 2],
        normal: Vec3::new(1.0, 0.0, 0.0),
        color: [0.08, 0.72, 1.00],
    },
    Face {
        corners: [3, 2, 6, 7],
        normal: Vec3::new(0.0, 1.0, 0.0),
        color: [0.98, 0.31, 0.70],
    },
    Face {
        corners: [4, 5, 1, 0],
        normal: Vec3::new(0.0, -1.0, 0.0),
        color: [0.20, 0.48, 0.92],
    },
];

fn build_showcase_mesh(
    output: &mut [Vertex; SHOWCASE_VERTEX_COUNT],
    frame: u32,
    width: u32,
    height: u32,
) {
    // Большой background quad даёт depth/atmosphere, но всё ещё проходит
    // через тот же GPU draw call, а не рисуется guest CPU во framebuffer.
    let backdrop = [
        Vertex::new([-1.0, -1.0, 0.95, 1.0], [0.015, 0.030, 0.100, 1.0]),
        Vertex::new([1.0, -1.0, 0.95, 1.0], [0.055, 0.018, 0.120, 1.0]),
        Vertex::new([1.0, 1.0, 0.95, 1.0], [0.010, 0.070, 0.130, 1.0]),
        Vertex::new([-1.0, -1.0, 0.95, 1.0], [0.015, 0.030, 0.100, 1.0]),
        Vertex::new([1.0, 1.0, 0.95, 1.0], [0.010, 0.070, 0.130, 1.0]),
        Vertex::new([-1.0, 1.0, 0.95, 1.0], [0.035, 0.012, 0.090, 1.0]),
    ];
    output[..backdrop.len()].copy_from_slice(&backdrop);

    let turns = frame as f32 / 240.0;
    let sy = sin_turns(turns);
    let cy = sin_turns(turns + 0.25);
    let sx = sin_turns(turns * 0.61 + 0.08) * 0.48;
    let cx = positive_sqrt(1.0 - sx * sx);
    let aspect = height as f32 / width as f32;
    let pulse = 1.0 + 0.055 * sin_turns(turns * 2.0);

    let mut transformed = [Vec3::new(0.0, 0.0, 0.0); 8];
    for (target, source) in transformed.iter_mut().zip(CUBE) {
        *target = source.rotate(sy, cy, sx, cx);
    }

    // Painter order достаточен для непрозрачного convex cube и позволяет не
    // вводить depth resource до следующего расширения winsys ABI.
    let mut order = [0usize, 1, 2, 3, 4, 5];
    let mut depth = [0.0f32; 6];
    for (index, face) in FACES.iter().enumerate() {
        depth[index] = face
            .corners
            .iter()
            .map(|corner| transformed[*corner].z)
            .sum::<f32>()
            * 0.25;
    }
    for left in 0..order.len() {
        for right in left + 1..order.len() {
            if depth[order[left]] > depth[order[right]] {
                order.swap(left, right);
            }
        }
    }

    let light = Vec3::new(-0.36, 0.67, 0.65);
    let indices = [0usize, 1, 2, 0, 2, 3];
    let mut cursor = backdrop.len();
    for face_index in order {
        let face = FACES[face_index];
        let normal = face.normal.rotate(sy, cy, sx, cx);
        let diffuse = (normal.x * light.x + normal.y * light.y + normal.z * light.z).max(0.0);
        let specular = diffuse * diffuse * diffuse * diffuse;
        let illumination = 0.17 + diffuse * 0.72 + specular * 0.32;
        let color = [
            (face.color[0] * illumination + specular * 0.18).min(1.0),
            (face.color[1] * illumination + specular * 0.22).min(1.0),
            (face.color[2] * illumination + specular * 0.28).min(1.0),
            1.0,
        ];
        for corner in indices {
            let point = transformed[face.corners[corner]];
            let camera_depth = 4.2 - point.z;
            output[cursor] = Vertex::new(
                [
                    point.x * 2.15 * aspect * pulse / camera_depth,
                    point.y * 2.15 * pulse / camera_depth,
                    0.05,
                    1.0,
                ],
                color,
            );
            cursor += 1;
        }
    }
}

/// Быстрая периодическая аппроксимация sinus без libm. Ошибка приемлема для
/// анимации и не влияет на layout/API; upstream Mesa позднее использует libm.
fn sin_turns(mut turns: f32) -> f32 {
    turns -= (turns as i32) as f32;
    if turns < 0.0 {
        turns += 1.0;
    }
    let mut value = if turns < 0.5 {
        4.0 * turns * (1.0 - turns)
    } else {
        let x = turns - 0.5;
        -4.0 * x * (1.0 - x)
    };
    let absolute = if value < 0.0 { -value } else { value };
    value *= 0.775 + 0.225 * absolute;
    value
}

fn positive_sqrt(value: f32) -> f32 {
    if value <= 0.0 {
        return 0.0;
    }
    let mut estimate = 1.0;
    for _ in 0..5 {
        estimate = 0.5 * (estimate + value / estimate);
    }
    estimate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context::new(
            VirglWinsysSurface {
                width: 1280,
                height: 800,
                color_resource: 41,
                vertex_resource: 42,
            },
            ApiProfile::OpenGlCore,
        )
        .unwrap()
    }

    #[test]
    fn showcase_is_bounded_and_changes_between_frames() {
        let mut first = [0u32; 768];
        let mut second = [0u32; 768];
        let mut context = context();
        let first_len = context.render_aurora_frame(&mut first, 0).unwrap();
        let second_len = context.render_aurora_frame(&mut second, 37).unwrap();
        assert!(first_len * 4 <= 3072);
        assert!(second_len * 4 <= 3072);
        assert!(second_len < first_len, "immutable pipeline must be reused");
        assert_ne!(&first[..first_len], &second[..second_len]);
    }

    #[test]
    fn invalid_winsys_surface_is_rejected() {
        assert_eq!(
            Context::new(
                VirglWinsysSurface {
                    width: 0,
                    height: 800,
                    color_resource: 1,
                    vertex_resource: 2,
                },
                ApiProfile::OpenGlCore,
            ),
            Err(MesaError::InvalidSurface)
        );
    }
}
