//! Surface queue, explicit synchronization и presentation feedback.
//!
//! Это wire-типы межпроцессного протокола, поэтому `rustos-video` использует
//! единственное каноническое определение из `rustos-abi`, а не копию.

pub use rustos_abi::surface::{
    commit_flags, feedback_flags, BufferReleased, DamageRect, OutputId, PresentMode,
    PresentationFeedback, PresentationStatus, SurfaceAbiError, SurfaceCommit, SurfaceCreateRequest,
    SurfaceId, SurfaceMetrics, SurfaceTransform, SURFACE_ABI_VERSION, SURFACE_MAX_DAMAGE_RECTS,
    SURFACE_MAX_QUEUE_DEPTH, SURFACE_MIN_QUEUE_DEPTH,
};
pub use rustos_abi::sync::{
    SyncAbiError, SyncPoint, SyncTimelineCreate, SyncTimelineSignal, SyncTimelineWait,
    SyncWaitMany, SyncWaitMode, SYNC_ABI_VERSION, SYNC_MAX_WAIT_POINTS, SYNC_TIMEOUT_INFINITE,
};
