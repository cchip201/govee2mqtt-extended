use crate::commands::serve::POLL_INTERVAL;
use crate::hass_mqtt::base::{Device, EntityConfig, Origin};
use crate::hass_mqtt::humidifier::DEVICE_CLASS_HUMIDITY;
use crate::hass_mqtt::instance::{publish_entity_config, EntityInstance};
use crate::platform_api::DeviceCapability;
use crate::service::device::Device as ServiceDevice;
use crate::service::hass::{availability_topic, topic_safe_id, topic_safe_string, HassClient};
use crate::service::quirks::HumidityUnits;
use crate::service::state::StateHandle;
use crate::temperature::{TemperatureUnits, TemperatureValue, DEVICE_CLASS_TEMPERATURE};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

#[derive(Serialize, Clone, Debug)]
pub struct SensorConfig {
    #[serde(flatten)]
    pub base: EntityConfig,

    pub state_topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_class: Option<StateClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_of_measurement: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_attributes_topic: Option<String>,
}

#[allow(unused)]
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateClass {
    #[serde(rename = "measurement")]
    Measurement,
    #[serde(rename = "total")]
    Total,
    #[serde(rename = "total_increasing")]
    TotalIncreasing,
}

impl SensorConfig {
    pub async fn publish(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        publish_entity_config("sensor", state, client, &self.base, self).await
    }

    pub async fn notify_state(&self, client: &HassClient, value: &str) -> anyhow::Result<()> {
        client.publish(&self.state_topic, value).await
    }
}

#[derive(Clone)]
pub struct GlobalFixedDiagnostic {
    sensor: SensorConfig,
    value: String,
}

#[async_trait]
impl EntityInstance for GlobalFixedDiagnostic {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.publish(state, client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.notify_state(client, &self.value).await
    }
}

impl GlobalFixedDiagnostic {
    pub fn new<NAME: Into<String>, VALUE: Into<String>>(name: NAME, value: VALUE) -> Self {
        let name = name.into();
        let unique_id = format!("global-{}", topic_safe_string(&name));

        Self {
            sensor: SensorConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some(name),
                    entity_category: Some("diagnostic".to_string()),
                    origin: Origin::default(),
                    device: Device::this_service(),
                    unique_id: unique_id.clone(),
                    device_class: None,
                    icon: None,
                },
                state_topic: format!("gv2mqtt/sensor/{unique_id}/state"),
                state_class: None,
                unit_of_measurement: None,
                json_attributes_topic: None,
            },
            value: value.into(),
        }
    }
}

#[derive(Clone)]
pub struct CapabilitySensor {
    sensor: SensorConfig,
    device_id: String,
    state: StateHandle,
    instance_name: String,
}

impl CapabilitySensor {
    pub async fn new(
        device: &ServiceDevice,
        state: &StateHandle,
        instance: &DeviceCapability,
    ) -> anyhow::Result<Self> {
        let unique_id = format!(
            "sensor-{id}-{inst}",
            id = topic_safe_id(device),
            inst = topic_safe_string(&instance.instance)
        );

        let unit_of_measurement = match instance.instance.as_str() {
            "sensorTemperature" => Some(state.get_temperature_scale().await.unit_of_measurement()),
            "sensorHumidity" => Some("%"),
            _ => None,
        };

        let device_class = match instance.instance.as_str() {
            "sensorTemperature" => Some(DEVICE_CLASS_TEMPERATURE),
            "sensorHumidity" => Some(DEVICE_CLASS_HUMIDITY),
            _ => None,
        };

        let state_class = match instance.instance.as_str() {
            "sensorTemperature" => Some(StateClass::Measurement),
            "sensorHumidity" => Some(StateClass::Measurement),
            _ => None,
        };

        let name = match instance.instance.as_str() {
            "sensorTemperature" => "Temperature".to_string(),
            "sensorHumidity" => "Humidity".to_string(),
            "online" => "Connected to Govee Cloud".to_string(),
            _ => instance.instance.to_string(),
        };

        Ok(Self {
            sensor: SensorConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some(name),
                    entity_category: Some("diagnostic".to_string()),
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id: unique_id.clone(),
                    device_class,
                    icon: None,
                },
                state_topic: format!("gv2mqtt/sensor/{unique_id}/state"),
                state_class,
                unit_of_measurement,
                json_attributes_topic: None,
            },
            device_id: device.id.to_string(),
            state: state.clone(),
            instance_name: instance.instance.to_string(),
        })
    }
}

#[async_trait]
impl EntityInstance for CapabilitySensor {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.publish(state, client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let Some(device) = self.state.device_by_id(&self.device_id).await else {
            log::warn!(
                "Device {} not found in state, skipping notify",
                self.device_id
            );
            return Ok(());
        };

        let quirk = device.resolve_quirk();

        if let Some(cap) = device.get_state_capability_by_instance(&self.instance_name) {
            let value = match self.instance_name.as_str() {
                "sensorTemperature" => {
                    let units = quirk
                        .and_then(|q| q.platform_temperature_sensor_units)
                        .unwrap_or(TemperatureUnits::Fahrenheit);

                    match cap
                        .state
                        .pointer("/value")
                        .and_then(|v| v.as_f64())
                        .map(|v| TemperatureValue::new(v, units))
                    {
                        Some(v) => {
                            let value = v
                                .as_unit(self.state.get_temperature_scale().await.into())
                                .value();
                            format!("{value:.2}")
                        }
                        None => "".to_string(),
                    }
                }
                "sensorHumidity" => {
                    let units = quirk
                        .and_then(|q| q.platform_humidity_sensor_units)
                        .unwrap_or(HumidityUnits::RelativePercent);
                    match cap
                        .state
                        .pointer("/value")
                        .and_then(|v| v.as_f64())
                        .map(|v| units.from_reading_to_relative_percent(v))
                    {
                        Some(v) => format!("{v:.2}"),
                        None => "".to_string(),
                    }
                }
                _ => cap.state.to_string(),
            };

            return self.sensor.notify_state(client, &value).await;
        }
        log::trace!(
            "CapabilitySensor::notify_state: didn't find state for {device} {instance}",
            instance = self.instance_name
        );
        Ok(())
    }
}

/// The Status sensor's state ("Available", "Missing" or "Unknown") and its attribute block.
///
/// Every device-state projection carries an `updated` stamp that moves on each poll. Left in
/// the attributes, it made Home Assistant record a new state row (with the whole 2-6 KB block)
/// every 20-30 seconds for a device whose content had not changed. The stamps stay out of the
/// attributes so the block only changes when the device's real state does; the summary still
/// reads `updated`, so Available/Missing keeps its meaning.
fn status_report(device: &ServiceDevice, now: DateTime<Utc>) -> (String, serde_json::Value) {
    let device_state = device.device_state();

    let threshold = *POLL_INTERVAL + chrono::Duration::seconds(30);

    let summary = match &device_state {
        Some(state) => {
            if now - state.updated > threshold {
                "Missing".to_string()
            } else {
                "Available".to_string()
            }
        }
        None => "Unknown".to_string(),
    };

    let mut attributes = json!({
        "iot": device.compute_iot_device_state(),
        "lan": device.compute_lan_device_state(),
        "http": device.compute_http_device_state(),
        "platform_metadata": &device.http_device_info,
        "platform_state": &device.http_device_state,
        "overall": device_state,
    });

    for projection in ["iot", "lan", "http", "overall"] {
        if let Some(fields) = attributes
            .get_mut(projection)
            .and_then(|value| value.as_object_mut())
        {
            fields.remove("updated");
        }
    }

    (summary, attributes)
}

pub struct DeviceStatusDiagnostic {
    sensor: SensorConfig,
    device_id: String,
    state: StateHandle,
}

impl DeviceStatusDiagnostic {
    pub fn new(device: &ServiceDevice, state: &StateHandle) -> Self {
        let unique_id = format!("sensor-{id}-gv2mqtt-status", id = topic_safe_id(device),);

        Self {
            sensor: SensorConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some("Status".to_string()),
                    entity_category: Some("diagnostic".to_string()),
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id: unique_id.clone(),
                    device_class: None,
                    icon: None,
                },
                state_topic: format!("gv2mqtt/sensor/{unique_id}/state"),
                state_class: None,
                json_attributes_topic: Some(format!("gv2mqtt/sensor/{unique_id}/attributes")),
                unit_of_measurement: None,
            },
            device_id: device.id.to_string(),
            state: state.clone(),
        }
    }
}

#[async_trait]
impl EntityInstance for DeviceStatusDiagnostic {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.publish(state, client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let Some(device) = self.state.device_by_id(&self.device_id).await else {
            log::warn!(
                "Device {} not found in state, skipping notify",
                self.device_id
            );
            return Ok(());
        };

        let (summary, attributes) = status_report(&device, Utc::now());

        self.sensor.notify_state(client, &summary).await?;
        if let Some(topic) = &self.sensor.json_attributes_topic {
            client.publish_obj(topic, attributes).await?;
        }
        Ok(())
    }
}

pub struct SceneInfoSensor {
    sensor: SensorConfig,
    device_id: String,
    device_topic_id: String,
    state: StateHandle,
}

impl SceneInfoSensor {
    pub fn new(device: &ServiceDevice, state: &StateHandle) -> Self {
        let unique_id = format!("sensor-{id}-gv2mqtt-scene-info", id = topic_safe_id(device));

        Self {
            device_topic_id: topic_safe_id(device),
            sensor: SensorConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some("Scene Info".to_string()),
                    entity_category: None,
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id: unique_id.clone(),
                    device_class: None,
                    icon: Some("mdi:palette".to_string()),
                },
                state_topic: format!("gv2mqtt/sensor/{unique_id}/state"),
                state_class: None,
                json_attributes_topic: Some(format!("gv2mqtt/sensor/{unique_id}/attributes")),
                unit_of_measurement: None,
            },
            device_id: device.id.to_string(),
            state: state.clone(),
        }
    }
}

#[async_trait]
impl EntityInstance for SceneInfoSensor {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.publish(state, client).await?;

        // Publish scene catalog as retained MQTT message during registration (once),
        // not on every state change. HA automations can subscribe to this topic.
        if let Some(device) = self.state.device_by_id(&self.device_id).await {
            let catalog = self
                .state
                .device_list_scenes_categorized(&device)
                .await
                .unwrap_or_default();
            if !catalog.is_empty() {
                let catalog_topic = format!("gv2mqtt/{}/scene-catalog", self.device_topic_id);
                if let Err(err) = client.publish_obj_retained(&catalog_topic, &catalog).await {
                    log::warn!("Failed to publish scene catalog: {err:#}");
                }
            }
        }

        Ok(())
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let Some(device) = self.state.device_by_id(&self.device_id).await else {
            return Ok(());
        };

        let active = device.active_scene_name();
        let scene_name = active.unwrap_or("None").to_string();

        let catalog = self
            .state
            .device_list_scenes_categorized(&device)
            .await
            .unwrap_or_default();

        // Build flat ordered list (entry + category) for index + thumbnail/hint lookup
        let flat: Vec<_> = catalog
            .iter()
            .flat_map(|cat| cat.scenes.iter().map(move |s| (s, cat.name.as_str())))
            .collect();

        // Only resolve an index when a scene is actually active; never match the
        // "None" sentinel against the catalog (a scene literally named "None" must
        // not look active when nothing is playing).
        let current_idx = active.and_then(|name| {
            flat.iter()
                .position(|(s, _)| s.name.eq_ignore_ascii_case(name))
        });

        // Keep `scene_name`, `category`, `index`, `total`, `next_scene`, `prev_scene`
        // exactly as before — existing HA automations, templates, and the user's
        // dashboard consume them. The new fields further down are strictly additive.
        let (category, index, next_scene, prev_scene) = if let Some(idx) = current_idx {
            let total = flat.len();
            let next_idx = (idx + 1) % total;
            let prev_idx = if idx == 0 { total - 1 } else { idx - 1 };
            (
                flat[idx].1.to_string(),
                idx,
                flat[next_idx].0.name.clone(),
                flat[prev_idx].0.name.clone(),
            )
        } else {
            let next = flat
                .first()
                .map(|(s, _)| s.name.clone())
                .unwrap_or_default();
            let prev = flat.last().map(|(s, _)| s.name.clone()).unwrap_or_default();
            ("Unknown".to_string(), 0, next, prev)
        };

        // Additive, render-ready fields for the v2 Scene Deck card. We publish data,
        // not UI strings — the card composes its display copy from `has_active_scene`,
        // `scene_name`, and `is_on`.
        let has_active_scene = active.is_some();
        let thumbnail = current_idx
            .and_then(|idx| flat[idx].0.icon_urls.first().cloned())
            .unwrap_or_default();
        let hint = current_idx
            .and_then(|idx| flat[idx].0.hint.clone())
            .unwrap_or_default();
        // Mirror the light entity's on/off semantics (see light.rs) so the card and the
        // light entity never disagree: an absent `light_on` (or no device state yet)
        // means "light off". Always a bool so consumers never see JSON null.
        let is_on = device
            .device_state()
            .and_then(|s| s.light_on)
            .unwrap_or(false);

        let attributes = json!({
            // existing contract — unchanged
            "scene_name": scene_name,
            "category": category,
            "index": index,
            "total": flat.len(),
            "next_scene": next_scene,
            "prev_scene": prev_scene,
            // additive
            "has_active_scene": has_active_scene,
            "thumbnail": thumbnail,
            "hint": hint,
            "is_on": is_on,
        });

        self.sensor.notify_state(client, &scene_name).await?;
        if let Some(topic) = &self.sensor.json_attributes_topic {
            client.publish_obj(topic, attributes).await?;
        }
        Ok(())
    }
}

/// What the rice cooker Program sensor publishes for a device, if anything.
///
/// The H7180 only reports on its account IoT topic while it is used, so
/// after a bridge restart `rice_cooker_program` is `None` until the next
/// report. An unknown program must not be published as an empty state:
/// it overwrites the value Home Assistant still holds (and the kiosk's
/// "Rice Cooker" live card then reads the blank as cooking). `None`
/// means "nothing to say", so the last published value stays.
#[derive(Debug, PartialEq)]
struct RiceCookerProgramReport {
    state: Option<String>,
    attributes: Option<serde_json::Value>,
}

fn rice_cooker_program_report(device: &ServiceDevice) -> RiceCookerProgramReport {
    let state = device
        .rice_cooker_program
        .map(crate::ble::rice_cooker_program_name);

    // The attribute block is all-null until something is reported; publishing
    // that would blank the attributes Home Assistant already holds.
    let attributes = (device.rice_cooker_program.is_some() || device.rice_cooker_params.is_some())
        .then(|| {
            let params_hex = device.rice_cooker_params.as_ref().map(|p| {
                p.params
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            json!({
                "program": device.rice_cooker_program,
                "phase": device.rice_cooker_phase,
                "params_program": device.rice_cooker_params.as_ref().map(|p| p.program),
                "params_hex": params_hex,
                "set_temperature_centi_fahrenheit": device
                    .rice_cooker_params
                    .as_ref()
                    .and_then(|p| p.set_temperature_centi_fahrenheit()),
                "duration_minutes": device
                    .rice_cooker_params
                    .as_ref()
                    .and_then(|p| p.duration_minutes()),
                "scheduled_finish_in_minutes": device
                    .rice_cooker_params
                    .as_ref()
                    .and_then(|p| p.scheduled_finish_in_minutes()),
            })
        });

    RiceCookerProgramReport { state, attributes }
}

/// The rice cooker's active program, decoded from the `AA 05 00` /
/// `AA 19` report frames on the account IoT topic (see the rice
/// cooker codecs in ble.rs for the capture evidence). Program ids are
/// numeric on purpose: which physical cooking function each id maps
/// to is not yet known, and naming them would be a guess. The raw
/// parameter block rides along as attributes so protocol work can
/// continue from live state.
pub struct RiceCookerProgramSensor {
    sensor: SensorConfig,
    device_id: String,
    state: StateHandle,
}

impl RiceCookerProgramSensor {
    pub fn new(device: &ServiceDevice, state: &StateHandle) -> Self {
        let unique_id = format!(
            "sensor-{id}-gv2mqtt-cooker-program",
            id = topic_safe_id(device)
        );

        Self {
            sensor: SensorConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some("Program".to_string()),
                    entity_category: None,
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id: unique_id.clone(),
                    device_class: None,
                    icon: Some("mdi:pot-steam".to_string()),
                },
                state_topic: format!("gv2mqtt/sensor/{unique_id}/state"),
                state_class: None,
                json_attributes_topic: Some(format!("gv2mqtt/sensor/{unique_id}/attributes")),
                unit_of_measurement: None,
            },
            device_id: device.id.to_string(),
            state: state.clone(),
        }
    }
}

#[async_trait]
impl EntityInstance for RiceCookerProgramSensor {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.publish(state, client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let Some(device) = self.state.device_by_id(&self.device_id).await else {
            log::warn!(
                "Device {} not found in state, skipping notify",
                self.device_id
            );
            return Ok(());
        };

        let report = rice_cooker_program_report(&device);
        if report.state.is_none() && report.attributes.is_none() {
            log::debug!(
                "Rice cooker {} has reported no program since the bridge started; \
                 publishing nothing",
                self.device_id
            );
            return Ok(());
        }

        if let Some(value) = &report.state {
            self.sensor.notify_state(client, value).await?;
        }
        if let (Some(topic), Some(attributes)) =
            (&self.sensor.json_attributes_topic, report.attributes)
        {
            client.publish_obj(topic, attributes).await?;
        }
        Ok(())
    }
}

/// The Set Temperature state to publish, or `None` when there is nothing to say.
///
/// `None` means no parameter block has arrived since the bridge started (the
/// cooker only reports when used): the last published value stays, rather than
/// being overwritten with an empty state. A block that did arrive but whose
/// program has no known temperature field still publishes an empty state, so a
/// stale temperature from another program is not left standing.
fn rice_cooker_set_temperature_state(
    device: &ServiceDevice,
    scale: crate::temperature::TemperatureScale,
) -> Option<String> {
    let params = device.rice_cooker_params.as_ref()?;
    Some(match params.set_temperature_centi_fahrenheit() {
        Some(centi_f) => {
            let temperature =
                TemperatureValue::new(f64::from(centi_f) / 100.0, TemperatureUnits::Fahrenheit);
            format!("{:.2}", temperature.as_unit(scale.into()).value())
        }
        None => "".to_string(),
    })
}

/// The set temperature of the rice cooker's current (or most recent)
/// program. The device reports hundredths of a degree Fahrenheit;
/// this converts to the service's configured temperature scale, the
/// same way the Platform API temperature sensors do.
pub struct RiceCookerSetTemperatureSensor {
    sensor: SensorConfig,
    device_id: String,
    state: StateHandle,
}

impl RiceCookerSetTemperatureSensor {
    pub async fn new(device: &ServiceDevice, state: &StateHandle) -> Self {
        let unique_id = format!(
            "sensor-{id}-gv2mqtt-cooker-set-temperature",
            id = topic_safe_id(device)
        );

        Self {
            sensor: SensorConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some("Set Temperature".to_string()),
                    entity_category: None,
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id: unique_id.clone(),
                    device_class: Some(DEVICE_CLASS_TEMPERATURE),
                    icon: None,
                },
                state_topic: format!("gv2mqtt/sensor/{unique_id}/state"),
                state_class: Some(StateClass::Measurement),
                json_attributes_topic: None,
                unit_of_measurement: Some(
                    state.get_temperature_scale().await.unit_of_measurement(),
                ),
            },
            device_id: device.id.to_string(),
            state: state.clone(),
        }
    }
}

#[async_trait]
impl EntityInstance for RiceCookerSetTemperatureSensor {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.publish(state, client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let Some(device) = self.state.device_by_id(&self.device_id).await else {
            log::warn!(
                "Device {} not found in state, skipping notify",
                self.device_id
            );
            return Ok(());
        };

        let scale = self.state.get_temperature_scale().await;
        let Some(value) = rice_cooker_set_temperature_state(&device, scale) else {
            log::debug!(
                "Rice cooker {} has reported no program parameters since the bridge started; \
                 publishing nothing",
                self.device_id
            );
            return Ok(());
        };

        self.sensor.notify_state(client, &value).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::lan_api::{DeviceColor, DeviceStatus as LanDeviceStatus};
    use crate::temperature::TemperatureScale;

    fn frame(brightness: u8) -> LanDeviceStatus {
        LanDeviceStatus {
            on: true,
            brightness,
            color: DeviceColor { r: 1, g: 2, b: 3 },
            color_temperature_kelvin: 0,
            mode: None,
        }
    }

    /// Two polls that report the same thing, a moment apart, must give the same attribute
    /// block: Home Assistant writes a state row whenever the attributes differ.
    #[test]
    fn repeated_poll_leaves_attributes_unchanged() {
        let mut device = ServiceDevice::new("H6000", "AA:BB:CC:DD:EE:FF:42:2A");
        device.set_lan_device_status(frame(100));
        let (first_summary, first) = status_report(&device, Utc::now());
        let first_stamp = device.device_state().expect("lan state").updated;

        std::thread::sleep(std::time::Duration::from_millis(5));
        device.set_lan_device_status(frame(100));
        let (second_summary, second) = status_report(&device, Utc::now());
        let second_stamp = device.device_state().expect("lan state").updated;

        assert!(second_stamp > first_stamp, "the poll moved its own stamp");
        assert_eq!(first, second);
        assert_eq!(first_summary, "Available");
        assert_eq!(second_summary, "Available");
    }

    /// The stamps are gone from every projection, and nothing else is.
    #[test]
    fn attributes_carry_no_poll_stamp_but_keep_the_content() {
        let mut device = ServiceDevice::new("H6000", "AA:BB:CC:DD:EE:FF:42:2A");
        device.set_lan_device_status(frame(100));
        let (_, attributes) = status_report(&device, Utc::now());

        for projection in ["iot", "lan", "http", "overall"] {
            if let Some(fields) = attributes[projection].as_object() {
                assert!(
                    !fields.contains_key("updated"),
                    "{projection} still has `updated`"
                );
            }
        }
        assert_eq!(attributes["lan"]["brightness"], 100);
        assert_eq!(attributes["lan"]["source"], "LAN API");
        assert_eq!(attributes["overall"]["brightness"], 100);
        assert!(attributes["iot"].is_null());
    }

    /// A real change in the device still changes the block, so the sensor still updates.
    #[test]
    fn real_change_still_changes_attributes() {
        let mut device = ServiceDevice::new("H6000", "AA:BB:CC:DD:EE:FF:42:2A");
        device.set_lan_device_status(frame(100));
        let (_, before) = status_report(&device, Utc::now());

        device.set_lan_device_status(frame(40));
        let (_, after) = status_report(&device, Utc::now());

        assert_ne!(before, after);
        assert_eq!(after["overall"]["brightness"], 40);
    }

    /// The summary still follows the age of the last poll, although that stamp is no longer
    /// part of the attributes.
    #[test]
    fn summary_follows_the_age_of_the_last_poll() {
        let unknown = ServiceDevice::new("H6000", "AA:BB:CC:DD:EE:FF:42:2A");
        assert_eq!(status_report(&unknown, Utc::now()).0, "Unknown");

        let mut device = ServiceDevice::new("H6000", "AA:BB:CC:DD:EE:FF:42:2A");
        device.set_lan_device_status(frame(100));
        let polled = device.device_state().expect("lan state").updated;

        let within = polled + *POLL_INTERVAL;
        assert_eq!(status_report(&device, within).0, "Available");

        let beyond = polled + *POLL_INTERVAL + chrono::Duration::seconds(31);
        assert_eq!(status_report(&device, beyond).0, "Missing");
    }

    fn cooker() -> ServiceDevice {
        ServiceDevice::new("H7180", "16:03:AA:BB:CC:DD:EE:FF")
    }

    /// Program 3 block of the 07:10:32 capture: set temperature 113.00 F (45 C).
    fn program_3_params() -> crate::ble::NotifyRiceCookerProgramParams {
        crate::ble::NotifyRiceCookerProgramParams {
            program: 3,
            params: [
                0x00, 0x46, 0x00, 0x00, 0x01, 0x2C, 0x24, 0x02, 0xD0, 0x01, 0x00, 0x00, 0x00, 0x00,
                0x01, 0x00,
            ],
        }
    }

    /// A bridge restart leaves the cooker with no program until it is used again.
    /// That must publish nothing, not an empty state over the value Home Assistant holds.
    #[test]
    fn unknown_program_publishes_nothing() {
        let report = rice_cooker_program_report(&cooker());
        assert_eq!(report.state, None);
        assert_eq!(report.attributes, None);
    }

    #[test]
    fn standby_is_a_real_value_and_is_published() {
        let mut device = cooker();
        device.set_rice_cooker_program(0);
        let report = rice_cooker_program_report(&device);
        assert_eq!(report.state.as_deref(), Some("Standby"));
        let attributes = report.attributes.expect("attributes with a known program");
        assert_eq!(attributes["program"], 0);
    }

    /// A parameter block without a program still has content worth publishing as
    /// attributes, but no state is invented for the program.
    #[test]
    fn params_without_a_program_publish_attributes_only() {
        let mut device = cooker();
        device.set_rice_cooker_params(program_3_params());
        let report = rice_cooker_program_report(&device);
        assert_eq!(report.state, None);
        let attributes = report.attributes.expect("attributes from the params block");
        assert_eq!(attributes["params_program"], 3);
        assert_eq!(attributes["set_temperature_centi_fahrenheit"], 11300);
    }

    #[test]
    fn unknown_set_temperature_publishes_nothing() {
        let device = cooker();
        for scale in [TemperatureScale::Celsius, TemperatureScale::Fahrenheit] {
            assert_eq!(rice_cooker_set_temperature_state(&device, scale), None);
        }
    }

    #[test]
    fn known_set_temperature_is_converted_to_the_scale() {
        let mut device = cooker();
        device.set_rice_cooker_params(program_3_params());
        let celsius = rice_cooker_set_temperature_state(&device, TemperatureScale::Celsius);
        let fahrenheit = rice_cooker_set_temperature_state(&device, TemperatureScale::Fahrenheit);
        assert_eq!(celsius.as_deref(), Some("45.00"));
        assert_eq!(fahrenheit.as_deref(), Some("113.00"));
    }

    /// The cooker did report, but for a program whose temperature field is not known:
    /// the empty state stays, so no stale temperature from another program is left up.
    #[test]
    fn reported_block_without_a_known_temperature_still_clears_it() {
        let mut device = cooker();
        let mut params = program_3_params();
        params.program = 9;
        device.set_rice_cooker_params(params);
        let shown = rice_cooker_set_temperature_state(&device, TemperatureScale::Celsius);
        assert_eq!(shown.as_deref(), Some(""));
    }
}
