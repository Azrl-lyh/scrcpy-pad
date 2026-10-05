import AccessibilityExtensionAbility from "@ohos:application.AccessibilityExtensionAbility";
import type { AccessibilityEvent } from "@ohos:application.AccessibilityExtensionAbility";
import type { KeyEvent } from "@ohos:multimodalInput.keyEvent";
import hilog from "@ohos:hilog";
import { AccessibilityBridge } from "@normalized:N&&&entry/src/main/ets/service/AccessibilityBridge&";
const DOMAIN = 0x5343;
export default class ScrcpyPadAccessibilityAbility extends AccessibilityExtensionAbility {
    onConnect(): void {
        AccessibilityBridge.attach(this.context);
        hilog.info(DOMAIN, 'Accessibility', 'connected');
    }
    onDisconnect(): void {
        AccessibilityBridge.detach();
        hilog.info(DOMAIN, 'Accessibility', 'disconnected');
    }
    onAccessibilityEvent(event: AccessibilityEvent): void {
    }
    onKeyEvent(keyEvent: KeyEvent): boolean {
        return AccessibilityBridge.handleKeyEvent(keyEvent);
    }
}
