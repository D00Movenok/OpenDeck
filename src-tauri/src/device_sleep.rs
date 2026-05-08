use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;

const SLEEP_REASON_IDLE: u8 = 1;
const SLEEP_REASON_COMPUTER_LOCKED: u8 = 1 << 1;
const SLEEP_REASON_MANUAL: u8 = 1 << 2;

static SLEEP_TIMEOUT_MINUTES: AtomicU16 = AtomicU16::new(0);
static SLEEP_WHEN_COMPUTER_LOCKED: AtomicBool = AtomicBool::new(false);
static COMPUTER_LOCKED: AtomicBool = AtomicBool::new(false);
static LAST_ACTIVITY: LazyLock<DashMap<String, Instant>> = LazyLock::new(DashMap::new);
static SLEEPING_DEVICES: LazyLock<DashMap<String, u8>> = LazyLock::new(DashMap::new);

pub fn init_device_sleep() {
	if let Ok(settings) = crate::store::get_settings() {
		SLEEP_TIMEOUT_MINUTES.store(settings.value.sleep_timeout_minutes, Ordering::Relaxed);
		SLEEP_WHEN_COMPUTER_LOCKED.store(settings.value.sleep_when_computer_locked, Ordering::Relaxed);
	}

	tokio::spawn(async {
		loop {
			if let Err(error) = sleep_idle_devices().await {
				log::warn!("Failed to update sleeping devices: {error}");
			}
			tokio::time::sleep(Duration::from_secs(2)).await;
		}
	});
}

pub async fn update_timeout_minutes(minutes: u16) -> Result<(), anyhow::Error> {
	SLEEP_TIMEOUT_MINUTES.store(minutes, Ordering::Relaxed);
	if minutes == 0 {
		for device in SLEEPING_DEVICES.iter().map(|entry| entry.key().clone()).collect::<Vec<_>>() {
			wake_device_reason(&device, SLEEP_REASON_IDLE).await?;
		}
	}
	Ok(())
}

pub async fn note_activity(device: &str) -> Result<bool, anyhow::Error> {
	LAST_ACTIVITY.insert(device.to_owned(), Instant::now());
	if COMPUTER_LOCKED.load(Ordering::Relaxed) && SLEEP_WHEN_COMPUTER_LOCKED.load(Ordering::Relaxed) {
		add_sleep_reason(device, SLEEP_REASON_COMPUTER_LOCKED).await?;
		return Ok(true);
	}

	wake_device_reason(device, SLEEP_REASON_IDLE).await
}

pub fn is_sleeping(device: &str) -> bool {
	SLEEPING_DEVICES.get(device).map(|entry| *entry.value() != 0).unwrap_or(false)
}

pub fn deregister_device(device: &str) {
	LAST_ACTIVITY.remove(device);
	SLEEPING_DEVICES.remove(device);
}

pub async fn sleep_device(device: String) -> Result<(), anyhow::Error> {
	add_sleep_reason(&device, SLEEP_REASON_MANUAL).await?;
	Ok(())
}

pub async fn wake_device(device: &str) -> Result<bool, anyhow::Error> {
	let was_sleeping = SLEEPING_DEVICES.remove(device).is_some();
	if was_sleeping {
		restore_brightness(device).await?;
	}
	Ok(was_sleeping)
}

pub async fn update_sleep_when_computer_locked(enabled: bool) -> Result<(), anyhow::Error> {
	SLEEP_WHEN_COMPUTER_LOCKED.store(enabled, Ordering::Relaxed);
	if enabled && COMPUTER_LOCKED.load(Ordering::Relaxed) {
		sleep_for_computer_lock().await?;
	} else if !enabled {
		wake_from_computer_lock().await?;
	}
	Ok(())
}

pub async fn sleep_for_computer_lock() -> Result<(), anyhow::Error> {
	COMPUTER_LOCKED.store(true, Ordering::Relaxed);
	if !SLEEP_WHEN_COMPUTER_LOCKED.load(Ordering::Relaxed) {
		return Ok(());
	}
	let device_ids = crate::shared::DEVICES.iter().map(|entry| entry.id.clone()).collect::<Vec<_>>();
	for device in device_ids {
		add_sleep_reason(&device, SLEEP_REASON_COMPUTER_LOCKED).await?;
	}
	Ok(())
}

pub async fn wake_from_computer_lock() -> Result<(), anyhow::Error> {
	COMPUTER_LOCKED.store(false, Ordering::Relaxed);
	let device_ids = SLEEPING_DEVICES.iter().map(|entry| entry.key().clone()).collect::<Vec<_>>();
	for device in device_ids {
		wake_device_reason(&device, SLEEP_REASON_COMPUTER_LOCKED).await?;
	}
	Ok(())
}

pub async fn apply_initial_device_sleep(device: &str) -> Result<(), anyhow::Error> {
	LAST_ACTIVITY.insert(device.to_owned(), Instant::now());
	if COMPUTER_LOCKED.load(Ordering::Relaxed) && SLEEP_WHEN_COMPUTER_LOCKED.load(Ordering::Relaxed) {
		add_sleep_reason(device, SLEEP_REASON_COMPUTER_LOCKED).await?;
	}
	Ok(())
}

async fn sleep_idle_devices() -> Result<(), anyhow::Error> {
	let timeout = SLEEP_TIMEOUT_MINUTES.load(Ordering::Relaxed);
	if timeout == 0 {
		return Ok(());
	}

	let idle_after = Duration::from_secs(timeout as u64 * 60);
	let now = Instant::now();
	let device_ids = LAST_ACTIVITY.iter().map(|entry| entry.key().clone()).collect::<Vec<_>>();

	for device in device_ids {
		let Some(last_activity) = LAST_ACTIVITY.get(&device).map(|entry| *entry.value()) else { continue };
		if now.duration_since(last_activity) < idle_after || is_sleeping(&device) {
			continue;
		}

		add_sleep_reason(&device, SLEEP_REASON_IDLE).await?;
	}

	Ok(())
}

async fn add_sleep_reason(device: &str, reason: u8) -> Result<(), anyhow::Error> {
	let reasons = SLEEPING_DEVICES.get(device).map(|entry| *entry.value()).unwrap_or(0);
	if reasons == 0 {
		crate::events::outbound::devices::set_device_brightness(device, 0).await?;
	}
	SLEEPING_DEVICES.insert(device.to_owned(), reasons | reason);
	Ok(())
}

async fn wake_device_reason(device: &str, reason: u8) -> Result<bool, anyhow::Error> {
	let Some(reasons) = SLEEPING_DEVICES.get(device).map(|entry| *entry.value()) else {
		return Ok(false);
	};
	let next = reasons & !reason;
	if next == reasons {
		return Ok(false);
	}
	if next == 0 {
		SLEEPING_DEVICES.remove(device);
		restore_brightness(device).await?;
	} else {
		SLEEPING_DEVICES.insert(device.to_owned(), next);
	}
	Ok(true)
}

async fn restore_brightness(device: &str) -> Result<(), anyhow::Error> {
	let brightness = crate::store::get_settings().map(|s| s.value.brightness).unwrap_or(50);
	crate::events::outbound::devices::set_device_brightness(device, brightness).await
}
