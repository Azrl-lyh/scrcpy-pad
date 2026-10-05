import type { AccessibilityExtensionContext } from '@kit.AccessibilityKit';
import type { KeyEvent } from '@ohos.multimodalInput.keyEvent';
import display from '@ohos.display';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { MappingEngine } from '../engine/MappingEngine';
import { AccessibilityGestureSink } from './AccessibilityGestureSink';
import { ConfigStore } from './ConfigStore';

const DOMAIN = 0x5343;
const KEY_ACTION_DOWN: number = 1;

export class AccessibilityBridge {
  private static context: AccessibilityExtensionContext | undefined = undefined;
  private static sink: AccessibilityGestureSink | undefined = undefined;
  private static engine: MappingEngine = new MappingEngine();

  static attach(context: AccessibilityExtensionContext): void {
    AccessibilityBridge.context = context;
    AccessibilityBridge.sink = new AccessibilityGestureSink(context);
    AccessibilityBridge.engine.setSink(AccessibilityBridge.sink);
    try {
      const info = display.getDefaultDisplaySync();
      AccessibilityBridge.engine.setViewport(info.width, info.height);
    } catch (error) {
      hilog.warn(DOMAIN, 'AccessibilityBridge', 'display size unavailable: %{public}s', JSON.stringify(error));
    }
    ConfigStore.attach(context);
    ConfigStore.loadInto(AccessibilityBridge.engine).catch((error: Error) => {
      hilog.warn(DOMAIN, 'AccessibilityBridge', 'settings load failed: %{public}s', JSON.stringify(error));
    });
    hilog.info(DOMAIN, 'AccessibilityBridge', 'accessibility engine attached');
  }

  static detach(): void {
    AccessibilityBridge.engine.cancelAll();
    AccessibilityBridge.sink = undefined;
    AccessibilityBridge.context = undefined;
  }

  static isAttached(): boolean {
    return AccessibilityBridge.context !== undefined;
  }

  static getEngine(): MappingEngine {
    return AccessibilityBridge.engine;
  }

  static handleKeyEvent(keyEvent: KeyEvent): boolean {
    const pressedCodes: number[] = [];
    for (let index = 0; index < keyEvent.keys.length; index += 1) {
      pressedCodes.push(keyEvent.keys[index].code as number);
    }
    const down = keyEvent.action === KEY_ACTION_DOWN;
    hilog.info(DOMAIN, 'KeyEvent', 'code=%{public}d action=%{public}d pressed=%{public}s',
      keyEvent.key.code as number, keyEvent.action as number, JSON.stringify(pressedCodes));
    return AccessibilityBridge.engine.handleKeyEvent(keyEvent.key.code as number, down, pressedCodes);
  }
}
