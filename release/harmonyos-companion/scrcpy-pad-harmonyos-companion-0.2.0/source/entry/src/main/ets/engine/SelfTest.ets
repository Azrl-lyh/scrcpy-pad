import { KeyAction, KeyBind, Point, WheelMode } from '../model/Keymap';
import { GestureSink } from './GestureSink';
import { MappingEngine } from './MappingEngine';

class RecordingSink implements GestureSink {
  tapCount: number = 0;
  beginHoldCount: number = 0;
  endHoldCount: number = 0;
  lastX: number = 0;
  lastY: number = 0;
  activeTokens: Set<string> = new Set<string>();

  tap(x: number, y: number, durationMs: number): void {
    this.tapCount += 1;
  }

  swipe(points: Point[], durationMs: number): void {
  }

  beginHold(token: string, x: number, y: number): void {
    this.activeTokens.add(token);
    this.beginHoldCount += 1;
    this.lastX = x;
    this.lastY = y;
  }

  moveHold(token: string, x: number, y: number): void {
    this.lastX = x;
    this.lastY = y;
  }

  endHold(token: string): void {
    this.activeTokens.delete(token);
    this.endHoldCount += 1;
  }

  cancelAll(): void {
    this.activeTokens.clear();
  }
}

export class SelfTest {
  static run(): string {
    let passed = 0;
    let total = 0;

    // 1. Auto-repeat on a held key must not replay a one-shot tap.
    const repeatEngine = new MappingEngine();
    const repeatSink = new RecordingSink();
    repeatEngine.setSink(repeatSink);
    repeatEngine.setEnabled(true);
    repeatEngine.replaceBindings([
      new KeyBind(2037, KeyAction.tap(0.5, 0.5, 40), false)
    ]);
    repeatEngine.handleKeyEvent(2037, true, [2037]);
    repeatEngine.handleKeyEvent(2037, true, [2037]);
    total += 1;
    if (repeatSink.tapCount === 1) {
      passed += 1;
    }

    // 2. Sensitive wheel: later direction wins per axis and pops back on release.
    const wheelEngine = new MappingEngine();
    const wheelSink = new RecordingSink();
    wheelEngine.setViewport(1000, 2000);
    wheelEngine.setSink(wheelSink);
    wheelEngine.setEnabled(true);
    wheelEngine.setAllWheelModes(WheelMode.SENSITIVE);
    wheelEngine.handleKeyEvent(2017, true, [2017]); // A: left
    const leftX = wheelSink.lastX;
    wheelEngine.handleKeyEvent(2020, true, [2017, 2020]); // D: later press overrides left
    const rightX = wheelSink.lastX;
    wheelEngine.handleKeyEvent(2020, false, [2017]); // release D: restore A
    const restoredX = wheelSink.lastX;
    wheelEngine.handleKeyEvent(2017, false, []); // release A: return center
    total += 1;
    if (leftX < rightX && restoredX < rightX && wheelSink.endHoldCount > 0) {
      passed += 1;
    }

    // 3. FPS-only binds work while the normal mapping is off.
    const fpsEngine = new MappingEngine();
    const fpsSink = new RecordingSink();
    fpsEngine.setSink(fpsSink);
    fpsEngine.setEnabled(false);
    fpsEngine.setFpsEnabled(true);
    fpsEngine.setFpsToggleKey(2098);
    fpsEngine.replaceBindings([
      new KeyBind(2037, KeyAction.tap(0.25, 0.75, 40), true)
    ]);
    fpsEngine.handleKeyEvent(2098, true, [2098]);
    fpsEngine.handleKeyEvent(2098, false, []);
    fpsEngine.handleKeyEvent(2037, true, [2037]);
    total += 1;
    if (fpsSink.tapCount === 1) {
      passed += 1;
    }

    // 4. A stale key absent from the authoritative pressed-key snapshot is healed.
    const staleEngine = new MappingEngine();
    const staleSink = new RecordingSink();
    staleEngine.setSink(staleSink);
    staleEngine.setEnabled(true);
    staleEngine.replaceBindings([
      new KeyBind(2037, KeyAction.hold(0.4, 0.4), false)
    ]);
    staleEngine.handleKeyEvent(2037, true, [2037]);
    staleEngine.handleKeyEvent(2027, true, [2027]); // K arrives without U: U release was lost
    total += 1;
    if (staleSink.beginHoldCount === 1 && staleSink.endHoldCount === 1 && staleSink.activeTokens.size === 0) {
      passed += 1;
    }

    return `核心状态机自检 ${passed}/${total} 通过`;
  }
}
