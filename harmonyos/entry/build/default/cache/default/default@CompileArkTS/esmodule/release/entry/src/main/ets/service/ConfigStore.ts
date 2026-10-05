import type common from "@ohos:app.ability.common";
import preferences from "@ohos:data.preferences";
import hilog from "@ohos:hilog";
import type { MappingEngine } from '../engine/MappingEngine';
import { EngineSettings, WheelMode } from "@normalized:N&&&entry/src/main/ets/model/Keymap&";
import type { KeyBind } from "@normalized:N&&&entry/src/main/ets/model/Keymap&";
const DOMAIN = 0x5343;
const STORE_NAME = 'scrcpy_pad_settings';
const KEY_MAPPING = 'mapping_enabled';
const KEY_WHEEL_MODE = 'wheel_mode';
const KEY_FPS = 'fps_enabled';
const KEY_FPS_TOGGLE = 'fps_toggle_key';
const KEY_FPS_SUSPEND = 'fps_suspend_key';
const KEY_HIDE_CURSOR = 'hide_cursor';
const KEY_BINDINGS = 'bindings_json';
export class ConfigStore {
    private static context: common.Context | undefined = undefined;
    private static watching: boolean = false;
    static attach(context: common.Context): void {
        ConfigStore.context = context;
    }
    static async loadInto(engine: MappingEngine): Promise<void> {
        const context = ConfigStore.context;
        if (context === undefined) {
            return;
        }
        try {
            const store = await preferences.getPreferences(context, STORE_NAME);
            if (!ConfigStore.watching) {
                ConfigStore.watching = true;
                store.on('multiProcessChange', (_key: string) => {
                    ConfigStore.loadInto(engine).catch((error: Error) => {
                        hilog.warn(DOMAIN, 'ConfigStore', 'sync failed: %{public}s', JSON.stringify(error));
                    });
                });
            }
            const settings = new EngineSettings();
            settings.mappingEnabled = await store.get(KEY_MAPPING, false) as boolean;
            const wheel = await store.get(KEY_WHEEL_MODE, WheelMode.CLASSIC) as string;
            settings.wheelMode = wheel === WheelMode.SENSITIVE ? WheelMode.SENSITIVE : WheelMode.CLASSIC;
            settings.fpsEnabled = await store.get(KEY_FPS, false) as boolean;
            settings.fpsToggleKey = await store.get(KEY_FPS_TOGGLE, 0) as number;
            settings.fpsSuspendKey = await store.get(KEY_FPS_SUSPEND, 0) as number;
            settings.hideCursor = await store.get(KEY_HIDE_CURSOR, true) as boolean;
            engine.applySettings(settings);
            const rawBindings = await store.get(KEY_BINDINGS, '[]') as string;
            try {
                const bindings = JSON.parse(rawBindings) as KeyBind[];
                engine.replaceBindings(bindings);
            }
            catch (parseError) {
                hilog.warn(DOMAIN, 'ConfigStore', 'bindings ignored: %{public}s', JSON.stringify(parseError));
            }
            hilog.info(DOMAIN, 'ConfigStore', 'settings loaded');
        }
        catch (error) {
            hilog.warn(DOMAIN, 'ConfigStore', 'load failed: %{public}s', JSON.stringify(error));
        }
    }
    static async saveFrom(engine: MappingEngine): Promise<void> {
        const context = ConfigStore.context;
        if (context === undefined) {
            return;
        }
        try {
            const settings = engine.getSettings();
            const store = await preferences.getPreferences(context, STORE_NAME);
            await store.put(KEY_MAPPING, settings.mappingEnabled);
            await store.put(KEY_WHEEL_MODE, settings.wheelMode);
            await store.put(KEY_FPS, settings.fpsEnabled);
            await store.put(KEY_FPS_TOGGLE, settings.fpsToggleKey);
            await store.put(KEY_FPS_SUSPEND, settings.fpsSuspendKey);
            await store.put(KEY_HIDE_CURSOR, settings.hideCursor);
            await store.put(KEY_BINDINGS, JSON.stringify(engine.getProfile().binds));
            await store.flush();
        }
        catch (error) {
            hilog.warn(DOMAIN, 'ConfigStore', 'save failed: %{public}s', JSON.stringify(error));
        }
    }
}
