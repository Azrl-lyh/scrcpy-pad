export enum WheelMode {
    CLASSIC = "classic",
    SENSITIVE = "sensitive"
}
export enum ActionKind {
    TAP = "tap",
    HOLD = "hold",
    SWIPE = "swipe",
    SYSTEM = "system"
}
export enum TempMode {
    HOLD = "hold",
    TOGGLE = "toggle"
}
export class Point {
    x: number;
    y: number;
    constructor(x: number, y: number) {
        this.x = x;
        this.y = y;
    }
}
export class KeyAction {
    kind: ActionKind;
    x: number = 0;
    y: number = 0;
    durationMs: number = 40;
    startX: number = 0;
    startY: number = 0;
    endX: number = 0;
    endY: number = 0;
    systemKeyCode: number = 0;
    constructor(kind: ActionKind) {
        this.kind = kind;
    }
    static tap(x: number, y: number, durationMs: number): KeyAction {
        const action = new KeyAction(ActionKind.TAP);
        action.x = x;
        action.y = y;
        action.durationMs = durationMs;
        return action;
    }
    static hold(x: number, y: number): KeyAction {
        const action = new KeyAction(ActionKind.HOLD);
        action.x = x;
        action.y = y;
        return action;
    }
    static swipe(startX: number, startY: number, endX: number, endY: number, durationMs: number): KeyAction {
        const action = new KeyAction(ActionKind.SWIPE);
        action.startX = startX;
        action.startY = startY;
        action.endX = endX;
        action.endY = endY;
        action.durationMs = durationMs;
        return action;
    }
    static system(keyCode: number): KeyAction {
        const action = new KeyAction(ActionKind.SYSTEM);
        action.systemKeyCode = keyCode;
        return action;
    }
}
export class KeyBind {
    key: number;
    action: KeyAction;
    fpsOnly: boolean;
    constructor(key: number, action: KeyAction, fpsOnly: boolean = false) {
        this.key = key;
        this.action = action;
        this.fpsOnly = fpsOnly;
    }
}
export class TempWheel {
    key: number;
    mode: TempMode;
    constructor(key: number, mode: TempMode) {
        this.key = key;
        this.mode = mode;
    }
}
export class WheelConfig {
    up: number;
    down: number;
    left: number;
    right: number;
    cx: number;
    cy: number;
    radius: number;
    scope: number;
    mode: WheelMode;
    temp: TempWheel | undefined;
    constructor(up: number, down: number, left: number, right: number, cx: number, cy: number, radius: number, scope: number, mode: WheelMode = WheelMode.CLASSIC, temp: TempWheel | undefined = undefined) {
        this.up = up;
        this.down = down;
        this.left = left;
        this.right = right;
        this.cx = cx;
        this.cy = cy;
        this.radius = radius;
        this.scope = scope;
        this.mode = mode;
        this.temp = temp;
    }
    pushPx(viewportWidth: number): number {
        return this.radius * Math.max(0.2, Math.min(4.0, this.scope)) * viewportWidth;
    }
}
export class FpsConfig {
    enabled: boolean = false;
    anchorX: number = 0;
    anchorY: number = 0;
    sensitivityX: number = 2.0;
    sensitivityY: number = 2.0;
    invertY: boolean = false;
    holdKey: number = 0;
    toggleKey: number = 0;
    suspendKey: number = 0;
    hideCursor: boolean = true;
}
export class Profile {
    formatVersion: number = 2;
    name: string = '默认配置';
    toggleKey: number = 2097;
    binds: KeyBind[] = [];
    wheels: WheelConfig[] = [];
    fps: FpsConfig = new FpsConfig();
}
export const KEY_F8: number = 2097;
export const KEY_F9: number = 2098;
export class EngineSettings {
    mappingEnabled: boolean = false;
    wheelMode: WheelMode = WheelMode.CLASSIC;
    fpsEnabled: boolean = false;
    fpsToggleKey: number = 0;
    fpsSuspendKey: number = 0;
    hideCursor: boolean = true;
}
export function createDefaultProfile(): Profile {
    const profile = new Profile();
    profile.toggleKey = KEY_F8;
    profile.wheels.push(new WheelConfig(2039, 2035, 2017, 2020, 0.278, 0.62, 150.0 / 1080.0, 1.0));
    return profile;
}
