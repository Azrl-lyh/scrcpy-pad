import type AbilityConstant from "@ohos:app.ability.AbilityConstant";
import ConfigurationConstant from "@ohos:app.ability.ConfigurationConstant";
import UIAbility from "@ohos:app.ability.UIAbility";
import type Want from "@ohos:app.ability.Want";
import hilog from "@ohos:hilog";
import type window from "@ohos:window";
import { AccessibilityBridge } from "@normalized:N&&&entry/src/main/ets/service/AccessibilityBridge&";
import { ConfigStore } from "@normalized:N&&&entry/src/main/ets/service/ConfigStore&";
const DOMAIN = 0x5343;
export default class EntryAbility extends UIAbility {
    onCreate(want: Want, launchParam: AbilityConstant.LaunchParam): void {
        ConfigStore.attach(this.context);
        ConfigStore.loadInto(AccessibilityBridge.getEngine()).catch((error: Error) => {
            hilog.warn(DOMAIN, 'EntryAbility', 'settings load failed: %{public}s', JSON.stringify(error));
        });
        try {
            this.context.getApplicationContext().setColorMode(ConfigurationConstant.ColorMode.COLOR_MODE_DARK);
        }
        catch (err) {
            hilog.warn(DOMAIN, 'EntryAbility', 'setColorMode failed: %{public}s', JSON.stringify(err));
        }
    }
    onWindowStageCreate(windowStage: window.WindowStage): void {
        windowStage.loadContent('pages/Index', (err) => {
            if (err.code) {
                hilog.error(DOMAIN, 'EntryAbility', 'loadContent failed: %{public}s', JSON.stringify(err));
            }
        });
    }
}
