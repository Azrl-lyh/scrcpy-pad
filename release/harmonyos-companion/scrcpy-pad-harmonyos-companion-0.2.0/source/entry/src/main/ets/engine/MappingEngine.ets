import { hilog } from '@kit.PerformanceAnalysisKit';
import {
  ActionKind,
  createDefaultProfile,
  KeyAction,
  KeyBind,
  Point,
  EngineSettings,
  Profile,
  TempMode,
  WheelConfig,
  WheelMode
} from '../model/Keymap';
import { GestureSink, NoopGestureSink } from './GestureSink';

const DOMAIN = 0x5343;
const MAX_CONCURRENT_KEYS: number = 8;
const DEVICE_MAX_POINTERS: number = 10;

export class FingerPool {
  down: boolean[] = [];
  count: number = 0;

  align(size: number): void {
    while (this.down.length < size) {
      this.down.push(false);
    }
  }

  isDown(index: number): boolean {
    return index >= 0 && index < this.down.length && this.down[index];
  }

  tryDown(index: number, others: number): boolean {
    if (this.isDown(index) || this.count >= MAX_CONCURRENT_KEYS || this.count + others >= DEVICE_MAX_POINTERS) {
      return false;
    }
    this.align(index + 1);
    this.down[index] = true;
    this.count += 1;
    return true;
  }

  release(index: number): boolean {
    if (!this.isDown(index)) {
      return false;
    }
    this.down[index] = false;
    this.count = Math.max(0, this.count - 1);
    return true;
  }

  freeAll(): void {
    for (let index = 0; index < this.down.length; index += 1) {
      this.down[index] = false;
    }
    this.count = 0;
  }
}

export class WheelState {
  pressed: boolean[] = [false, false, false, false];
  pressSeq: number[] = [0, 0, 0, 0];
  nextSeq: number = 0;
  active: boolean = true;
  down: boolean = false;
  token: string = '';
  lastX: number = 0;
  lastY: number = 0;
}

export function wheelAxisValue(mode: WheelMode, negDown: boolean, posDown: boolean,
  negSeq: number, posSeq: number): number {
  if (!negDown && !posDown) {
    return 0;
  }
  if (negDown && !posDown) {
    return -1;
  }
  if (!negDown && posDown) {
    return 1;
  }
  if (mode === WheelMode.CLASSIC) {
    return 0;
  }
  return posSeq > negSeq ? 1 : -1;
}

export class MappingEngine {
  private profile: Profile = createDefaultProfile();
  private sink: GestureSink = new NoopGestureSink();
  private held: Set<number> = new Set<number>();
  private normalFingers: FingerPool = new FingerPool();
  private fpsFingers: FingerPool = new FingerPool();
  private wheelStates: WheelState[] = [];
  private tapToggleDown: boolean[] = [];
  private enabled: boolean = false;
  private fpsActive: boolean = false;
  private fpsSuspended: boolean = false;
  private viewportWidth: number = 1080;
  private viewportHeight: number = 2340;

  setSink(sink: GestureSink): void {
    this.cancelAll();
    this.sink = sink;
  }

  setViewport(width: number, height: number): void {
    this.viewportWidth = Math.max(1, width);
    this.viewportHeight = Math.max(1, height);
  }

  getProfile(): Profile {
    return this.profile;
  }

  getSettings(): EngineSettings {
    const settings = new EngineSettings();
    settings.mappingEnabled = this.enabled;
    if (this.profile.wheels.length > 0) {
      settings.wheelMode = this.profile.wheels[0].mode;
    }
    settings.fpsEnabled = this.profile.fps.enabled;
    settings.fpsToggleKey = this.profile.fps.toggleKey;
    settings.fpsSuspendKey = this.profile.fps.suspendKey;
    settings.hideCursor = this.profile.fps.hideCursor;
    return settings;
  }

  applySettings(settings: EngineSettings): void {
    this.enabled = settings.mappingEnabled;
    this.profile.fps.enabled = settings.fpsEnabled;
    this.profile.fps.toggleKey = settings.fpsToggleKey;
    this.profile.fps.suspendKey = settings.fpsSuspendKey;
    this.profile.fps.hideCursor = settings.hideCursor;
    if (!this.profile.fps.enabled) {
      this.fpsActive = false;
      this.fpsSuspended = false;
    }
    for (let index = 0; index < this.profile.wheels.length; index += 1) {
      this.profile.wheels[index].mode = settings.wheelMode;
    }
    this.releaseNormalBinds();
    this.releaseFpsBinds();
    this.releaseWheels();
    this.reconcileAll();
  }

  notifyProfileChanged(): void {
    this.releaseNormalBinds();
    this.releaseFpsBinds();
    this.releaseWheels();
    this.reconcileAll();
  }

  addTapBinding(key: number, x: number, y: number, fpsOnly: boolean): void {
    const normalizedX = Math.max(0, Math.min(1, x));
    const normalizedY = Math.max(0, Math.min(1, y));
    this.profile.binds.push(new KeyBind(key, KeyAction.tap(normalizedX, normalizedY, 40), fpsOnly));
    this.notifyProfileChanged();
  }

  removeBinding(index: number): void {
    if (index < 0 || index >= this.profile.binds.length) {
      return;
    }
    this.profile.binds.splice(index, 1);
    this.tapToggleDown.splice(index, 1);
    this.normalFingers.down.splice(index, 1);
    this.fpsFingers.down.splice(index, 1);
    this.notifyProfileChanged();
  }

  replaceBindings(bindings: KeyBind[]): void {
    const normalized: KeyBind[] = [];
    for (let index = 0; index < bindings.length; index += 1) {
      const item = bindings[index];
      if (item === undefined || item.action === undefined) {
        continue;
      }
      const action = new KeyAction(item.action.kind);
      action.x = item.action.x;
      action.y = item.action.y;
      action.durationMs = item.action.durationMs;
      action.startX = item.action.startX;
      action.startY = item.action.startY;
      action.endX = item.action.endX;
      action.endY = item.action.endY;
      action.systemKeyCode = item.action.systemKeyCode;
      normalized.push(new KeyBind(item.key, action, item.fpsOnly === true));
    }
    this.profile.binds = normalized;
    this.normalFingers = new FingerPool();
    this.fpsFingers = new FingerPool();
    this.tapToggleDown = [];
    this.notifyProfileChanged();
  }

  isEnabled(): boolean {
    return this.enabled;
  }

  setEnabled(enabled: boolean): void {
    if (this.enabled === enabled) {
      return;
    }
    this.enabled = enabled;
    if (!enabled) {
      this.releaseNormalBinds();
      this.releaseWheels();
    } else {
      this.reconcileAll();
    }
  }

  isFpsActive(): boolean {
    return this.fpsActive && this.isFpsRunning();
  }

  setFpsEnabled(enabled: boolean): void {
    this.profile.fps.enabled = enabled;
    if (!enabled) {
      this.fpsActive = false;
      this.fpsSuspended = false;
      this.releaseFpsBinds();
    } else {
      this.reconcileAll();
    }
  }

  setFpsToggleKey(key: number): void {
    this.profile.fps.toggleKey = key;
  }

  setFpsSuspendKey(key: number): void {
    this.profile.fps.suspendKey = key;
  }

  setFpsHideCursor(hide: boolean): void {
    this.profile.fps.hideCursor = hide;
  }

  setAllWheelModes(mode: WheelMode): void {
    for (let index = 0; index < this.profile.wheels.length; index += 1) {
      this.profile.wheels[index].mode = mode;
    }
    this.releaseWheels();
    this.reconcileAll();
  }

  setFirstWheelMode(mode: WheelMode): void {
    if (this.profile.wheels.length > 0) {
      this.profile.wheels[0].mode = mode;
    }
    this.releaseWheels();
    this.reconcileAll();
  }

  handleKeyEvent(code: number, down: boolean, pressedCodes: number[]): boolean {
    const wasDown = this.held.has(code);
    const freshPress = down && !wasDown;
    this.reconcileHeld(code, down, pressedCodes);

    let consumed = false;
    if (code === this.profile.toggleKey) {
      consumed = true;
      if (freshPress) {
        this.setEnabled(!this.enabled);
      }
    }

    if (this.profile.fps.enabled && this.profile.fps.toggleKey !== 0 && code === this.profile.fps.toggleKey) {
      consumed = true;
      if (freshPress) {
        this.fpsActive = !this.fpsActive;
        if (this.fpsActive) {
          this.fpsSuspended = false;
        }
        this.reconcileAll();
      }
    }

    if (this.profile.fps.enabled && this.profile.fps.suspendKey !== 0 && code === this.profile.fps.suspendKey) {
      consumed = true;
    }

    if (this.profile.fps.enabled && this.profile.fps.suspendKey !== 0) {
      const wantSuspend = this.held.has(this.profile.fps.suspendKey);
      if (wantSuspend !== this.fpsSuspended) {
        this.fpsSuspended = wantSuspend;
        this.reconcileAll();
      }
    }

    if (this.handleTemporaryWheelToggle(code, freshPress)) {
      consumed = true;
    }

    this.reconcileWheelDirections();
    const wheelOwned = this.isKeyOwnedByWheel(code);

    if (this.isWheelDirectionKey(code) || wheelOwned) {
      consumed = true;
    }

    this.reconcileNormalBinds();
    if (this.isFpsRunning()) {
      this.reconcileFpsBinds();
    } else {
      this.releaseFpsBinds();
    }

    if (freshPress) {
      consumed = this.fireOneShotBinds(code) || consumed;
    }

    this.reconcileNormalBinds();
    if (this.isFpsRunning()) {
      this.reconcileFpsBinds();
    } else {
      this.releaseFpsBinds();
    }
    return consumed;
  }

  cancelAll(): void {
    this.normalFingers.freeAll();
    this.fpsFingers.freeAll();
    this.held.clear();
    for (let index = 0; index < this.wheelStates.length; index += 1) {
      this.wheelStates[index].down = false;
    }
    this.sink.cancelAll();
  }

  private reconcileHeld(code: number, down: boolean, pressedCodes: number[]): void {
    if (pressedCodes.length > 0) {
      const current = new Set<number>();
      for (let index = 0; index < pressedCodes.length; index += 1) {
        current.add(pressedCodes[index]);
      }
      const previous: number[] = Array.from(this.held);
      for (let index = 0; index < previous.length; index += 1) {
        if (!current.has(previous[index])) {
          this.held.delete(previous[index]);
        }
      }
    }
    if (down) {
      this.held.add(code);
    } else {
      this.held.delete(code);
    }
  }

  private isFpsRunning(): boolean {
    return this.profile.fps.enabled && this.fpsActive && !this.fpsSuspended;
  }

  private reconcileAll(): void {
    this.alignState();
    this.reconcileTemporaryWheels();
    this.reconcileWheelDirections();
    this.reconcileNormalBinds();
    if (this.isFpsRunning()) {
      this.reconcileFpsBinds();
    } else {
      this.releaseFpsBinds();
    }
  }

  private alignState(): void {
    this.normalFingers.align(this.profile.binds.length);
    this.fpsFingers.align(this.profile.binds.length);
    while (this.tapToggleDown.length < this.profile.binds.length) {
      this.tapToggleDown.push(false);
    }
    while (this.wheelStates.length < this.profile.wheels.length) {
      const state = new WheelState();
      state.token = `wheel:${this.wheelStates.length}`;
      this.wheelStates.push(state);
    }
  }

  private handleTemporaryWheelToggle(code: number, freshPress: boolean): boolean {
    let consumed = false;
    for (let index = 0; index < this.profile.wheels.length; index += 1) {
      const temp = this.profile.wheels[index].temp;
      if (temp === undefined || temp.key !== code) {
        continue;
      }
      consumed = true;
      if (freshPress && temp.mode === TempMode.TOGGLE) {
        this.wheelStates[index].active = !this.wheelStates[index].active;
      }
    }
    return consumed;
  }

  private reconcileTemporaryWheels(): void {
    for (let index = 0; index < this.profile.wheels.length; index += 1) {
      const temp = this.profile.wheels[index].temp;
      if (temp === undefined) {
        this.wheelStates[index].active = true;
      } else if (temp.mode === TempMode.HOLD) {
        this.wheelStates[index].active = this.held.has(temp.key);
      }
    }
  }

  private reconcileWheelDirections(): void {
    for (let index = 0; index < this.profile.wheels.length; index += 1) {
      const wheel = this.profile.wheels[index];
      const state = this.wheelStates[index];
      const engaged = wheel.temp === undefined || state.active;
      const keys: number[] = [wheel.up, wheel.down, wheel.left, wheel.right];
      for (let direction = 0; direction < keys.length; direction += 1) {
        const want = engaged && this.held.has(keys[direction]);
        if (want && !state.pressed[direction]) {
          state.nextSeq += 1;
          state.pressSeq[direction] = state.nextSeq;
        }
        state.pressed[direction] = want;
      }
      this.updateWheel(index);
    }
  }

  private updateWheel(index: number): void {
    const wheel = this.profile.wheels[index];
    const state = this.wheelStates[index];
    const dx = wheelAxisValue(wheel.mode, state.pressed[2], state.pressed[3],
      state.pressSeq[2], state.pressSeq[3]);
    const dy = wheelAxisValue(wheel.mode, state.pressed[0], state.pressed[1],
      state.pressSeq[0], state.pressSeq[1]);
    if (dx === 0 && dy === 0) {
      if (state.down) {
        this.sink.endHold(state.token);
        state.down = false;
      }
      return;
    }

    let factorX = dx;
    let factorY = dy;
    if (dx !== 0 && dy !== 0) {
      const inv = 1.0 / Math.sqrt(2.0);
      factorX = dx * inv;
      factorY = dy * inv;
    }
    const centerX = wheel.cx * this.viewportWidth;
    const centerY = wheel.cy * this.viewportHeight;
    const targetX = centerX + factorX * wheel.pushPx(this.viewportWidth);
    const targetY = centerY + factorY * wheel.pushPx(this.viewportWidth);
    state.lastX = targetX;
    state.lastY = targetY;
    if (!state.down) {
      state.down = true;
      this.sink.beginHold(state.token, targetX, targetY);
    } else {
      this.sink.moveHold(state.token, targetX, targetY);
    }
  }

  private isWheelDirectionKey(code: number): boolean {
    for (let index = 0; index < this.profile.wheels.length; index += 1) {
      const wheel = this.profile.wheels[index];
      const engaged = wheel.temp === undefined || this.wheelStates[index].active;
      if (engaged && (wheel.up === code || wheel.down === code || wheel.left === code || wheel.right === code)) {
        return true;
      }
    }
    return false;
  }

  private isKeyOwnedByWheel(code: number): boolean {
    for (let index = 0; index < this.profile.wheels.length; index += 1) {
      const wheel = this.profile.wheels[index];
      const temp = wheel.temp;
      if (temp !== undefined && temp.key === code) {
        return true;
      }
      const engaged = temp === undefined || this.wheelStates[index].active;
      if (engaged && (wheel.up === code || wheel.down === code || wheel.left === code || wheel.right === code)) {
        return true;
      }
    }
    return false;
  }

  private fireOneShotBinds(code: number): boolean {
    let consumed = false;
    const fpsRunning = this.isFpsRunning();
    for (let index = 0; index < this.profile.binds.length; index += 1) {
      const bind = this.profile.binds[index];
      const laneActive = bind.fpsOnly ? fpsRunning : this.enabled;
      if (!laneActive || bind.key !== code || this.isKeyOwnedByWheel(code)) {
        continue;
      }
      if (bind.action.kind === ActionKind.TAP) {
        consumed = true;
        if (bind.action.durationMs === 0) {
          this.tapToggleDown[index] = !this.tapToggleDown[index];
          this.toggleTapPointer(index, bind);
        } else {
          this.sink.tap(this.pointX(bind.action.x), this.pointY(bind.action.y), bind.action.durationMs);
        }
      } else if (bind.action.kind === ActionKind.SWIPE) {
        consumed = true;
        this.sink.swipe([
          new Point(this.pointX(bind.action.startX), this.pointY(bind.action.startY)),
          new Point(this.pointX(bind.action.endX), this.pointY(bind.action.endY))
        ], bind.action.durationMs);
      }
    }
    return consumed;
  }

  private reconcileNormalBinds(): void {
    this.reconcileBinds(false);
  }

  private reconcileFpsBinds(): void {
    this.reconcileBinds(true);
  }

  private reconcileBinds(fpsLane: boolean): void {
    const pool = fpsLane ? this.fpsFingers : this.normalFingers;
    const laneActive = fpsLane ? this.isFpsRunning() : this.enabled;
    const others = this.countWheelPointers() + (fpsLane ? 0 : this.fpsFingers.count);
    for (let index = 0; index < this.profile.binds.length; index += 1) {
      const bind = this.profile.binds[index];
      if (bind.fpsOnly !== fpsLane) {
        continue;
      }
      if (bind.action.kind !== ActionKind.HOLD && bind.action.kind !== ActionKind.SYSTEM) {
        continue;
      }
      const want = laneActive && this.held.has(bind.key) && !this.isKeyOwnedByWheel(bind.key);
      if (want) {
        if (!pool.isDown(index) && pool.tryDown(index, others)) {
          if (bind.action.kind === ActionKind.HOLD) {
            this.sink.beginHold(this.bindToken(fpsLane, index), this.pointX(bind.action.x), this.pointY(bind.action.y));
          } else {
            hilog.info(DOMAIN, 'MappingEngine', 'system key %{public}d pressed', bind.action.systemKeyCode);
          }
        }
      } else if (pool.release(index)) {
        if (bind.action.kind === ActionKind.HOLD) {
          this.sink.endHold(this.bindToken(fpsLane, index));
        } else {
          hilog.info(DOMAIN, 'MappingEngine', 'system key %{public}d released', bind.action.systemKeyCode);
        }
      }
    }
  }

  private releaseNormalBinds(): void {
    this.releaseBinds(false);
  }

  private releaseFpsBinds(): void {
    this.releaseBinds(true);
  }

  private releaseBinds(fpsLane: boolean): void {
    const pool = fpsLane ? this.fpsFingers : this.normalFingers;
    for (let index = 0; index < this.profile.binds.length; index += 1) {
      const bind = this.profile.binds[index];
      if (bind.fpsOnly !== fpsLane || !pool.release(index)) {
        continue;
      }
      if (bind.action.kind === ActionKind.HOLD) {
        this.sink.endHold(this.bindToken(fpsLane, index));
      }
    }
  }

  private releaseWheels(): void {
    for (let index = 0; index < this.wheelStates.length; index += 1) {
      if (this.wheelStates[index].down) {
        this.sink.endHold(this.wheelStates[index].token);
      }
      this.wheelStates[index].down = false;
      this.wheelStates[index].pressed = [false, false, false, false];
    }
  }

  private countWheelPointers(): number {
    let count = 0;
    for (let index = 0; index < this.wheelStates.length; index += 1) {
      if (this.wheelStates[index].down) {
        count += 1;
      }
    }
    return count;
  }

  private toggleTapPointer(index: number, bind: KeyBind): void {
    const token = `tap-toggle:${index}`;
    if (this.tapToggleDown[index]) {
      this.sink.beginHold(token, this.pointX(bind.action.x), this.pointY(bind.action.y));
    } else {
      this.sink.endHold(token);
    }
  }

  private pointX(relative: number): number {
    return relative * this.viewportWidth;
  }

  private pointY(relative: number): number {
    return relative * this.viewportHeight;
  }

  private bindToken(fpsLane: boolean, index: number): string {
    return `${fpsLane ? 'fps' : 'bind'}:${index}`;
  }
}
