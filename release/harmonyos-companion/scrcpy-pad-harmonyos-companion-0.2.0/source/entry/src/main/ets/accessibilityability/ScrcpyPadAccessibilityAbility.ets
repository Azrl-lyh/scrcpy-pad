import { AccessibilityExtensionAbility, AccessibilityEvent } from '@kit.AccessibilityKit';
import type { KeyEvent } from '@ohos.multimodalInput.keyEvent';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { AccessibilityBridge } from '../service/AccessibilityBridge';

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
