//! one Vulkan device for the renderer and presenter when they'd pick the same GPU,
//! so the screens the renderer draws get shown straight from its images instead of copied out to the host and back
//! up

use ash::vk;

/// a device, the instance it came from and its queues, gone once the last of the renderer and presenter lets go of it
/// the renderer (on the emulation's thread) and the presenter (on the window's) each have a queue of their own when the
/// GPU's family has two (ZAKURO_ONE_QUEUE=1 keeps one): the presenter's work then doesn't wait behind the renderer's
/// batches, which made a present miss its vblank. Timeline semaphores order them: the renderer signals `drawn` w/ each
/// batch (its number + 1), the presenter waits for the batch that drew the pictures it shows and signals `shown` w/
/// each submit, and a renderer batch that turns a picture upright again waits for the presents submitted b4 it.
/// W/ one queue the order of submission does all that, and the two take turns at it thru `queue_lock`
pub struct SharedDevice {
    pub(crate) _entry: ash::Entry,
    pub(crate) instance: ash::Instance,
    pub(crate) physical: vk::PhysicalDevice,
    pub(crate) family: u32,
    pub(crate) device: ash::Device,
    pub(crate) queue: vk::Queue,
    /// the presenter's queue, the renderer's own when there's one
    pub(crate) present_queue: vk::Queue,
    /// Vulkan wants a queue's submits and presents, and the device's waits for idle, one thread at a time
    pub(crate) queue_lock: std::sync::Mutex<()>,
    pub(crate) present_lock: std::sync::Mutex<()>,
    /// w/ two queues: the timeline semaphores, and the value of the last `shown` submitted
    pub(crate) drawn: vk::Semaphore,
    pub(crate) shown: vk::Semaphore,
    pub(crate) shown_value: std::sync::atomic::AtomicU64,
    /// per image a picture was turned upright into (by its view), the `drawn` value of the last batch that did: a
    /// present reading that image waits for it too, so one made from a set picked b4 a rewrite doesn't read the image
    /// while it's written; presents of other images (the other slot) don't wait for it (both under queue_lock)
    pub(crate) rewritten: std::sync::Mutex<std::collections::HashMap<u64, u64>>,
    /// does the device do logic ops, and count pipeline statistics
    pub(crate) logic_ops: bool,
    pub(crate) statistics: bool,
    /// the blend state draws set as they go
    pub(crate) dynamic: crate::raster::hardware::Dynamic,
}

impl SharedDevice {
    /// were any of these pictures drawn over since, a later picture turned upright into their image: a present of them
    /// would show that one in their place (or wait for it). One drawn over right after this is asked is caught by the
    /// present's wait for its rewrite
    pub(crate) fn stale<'a>(&self, screens: impl IntoIterator<Item = &'a crate::GpuScreen>) -> bool {
        let rewritten = self.rewritten.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        screens.into_iter().any(|screen| rewritten.get(&ash::vk::Handle::as_raw(screen.view)).is_some_and(|&at| at > screen.drawn))
    }

    /// the renderer's queue to oneself while the guard lives
    pub(crate) fn lock_queue(&self) -> std::sync::MutexGuard<'_, ()> {
        self.queue_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// do the renderer and the presenter have a queue each
    pub(crate) fn two_queues(&self) -> bool {
        self.present_queue != self.queue
    }

    /// the presenter's queue to oneself while the guard lives
    pub(crate) fn lock_present(&self) -> std::sync::MutexGuard<'_, ()> {
        match self.two_queues() {
            true => self.present_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            false => self.lock_queue(),
        }
    }

    /// every queue to oneself, for waiting for the device to go idle (always in this order)
    pub(crate) fn lock_all(&self) -> (std::sync::MutexGuard<'_, ()>, Option<std::sync::MutexGuard<'_, ()>>) {
        let first = self.lock_queue();
        let second = self.two_queues().then(|| self.present_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        (first, second)
    }

    /// a device on the GPU the renderer would pick, w/ no presenter, to time showing the screens straight from the GPU w/o a window
    pub fn new() -> Result<SharedDevice, String> {
        crate::raster::hardware::own_device()
    }
}

impl std::fmt::Debug for SharedDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedDevice").field("family", &self.family).finish_non_exhaustive()
    }
}

impl Drop for SharedDevice {
    fn drop(&mut self) {
        // safe bc whoever made something from the device destroyed it b4 letting go, the instance's surface went w/ the presenter
        unsafe {
            let _ = self.device.device_wait_idle();
            for semaphore in [self.drawn, self.shown] {
                if semaphore != vk::Semaphore::null() {
                    self.device.destroy_semaphore(semaphore, None);
                }
            }
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
