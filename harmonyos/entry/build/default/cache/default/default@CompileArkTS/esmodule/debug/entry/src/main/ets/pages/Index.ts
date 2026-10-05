if (!("finalizeConstruction" in ViewPU.prototype)) {
    Reflect.set(ViewPU.prototype, "finalizeConstruction", () => { });
}
interface Index_Params {
    engine?;
    mappingEnabled?: boolean;
    fpsEnabled?: boolean;
    fpsToggleKey?: number;
    fpsSuspendKey?: number;
    hideCursor?: boolean;
    wheelMode?: WheelMode;
    statusText?: string;
    selfTestText?: string;
    bindRows?: string[];
    bindKey?: number;
    bindX?: number;
    bindY?: number;
    bindFpsOnly?: boolean;
}
import hilog from "@ohos:hilog";
import { AccessibilityBridge } from "@normalized:N&&&entry/src/main/ets/service/AccessibilityBridge&";
import { WheelMode } from "@normalized:N&&&entry/src/main/ets/model/Keymap&";
import { SelfTest } from "@normalized:N&&&entry/src/main/ets/engine/SelfTest&";
import { ConfigStore } from "@normalized:N&&&entry/src/main/ets/service/ConfigStore&";
const DOMAIN = 0x5343;
class Index extends ViewPU {
    constructor(parent, params, __localStorage, elmtId = -1, paramsLambda = undefined, extraInfo) {
        super(parent, __localStorage, elmtId, extraInfo);
        if (typeof paramsLambda === "function") {
            this.paramsGenerator_ = paramsLambda;
        }
        this.engine = AccessibilityBridge.getEngine();
        this.__mappingEnabled = new ObservedPropertySimplePU(false, this, "mappingEnabled");
        this.__fpsEnabled = new ObservedPropertySimplePU(false, this, "fpsEnabled");
        this.__fpsToggleKey = new ObservedPropertySimplePU(0, this, "fpsToggleKey");
        this.__fpsSuspendKey = new ObservedPropertySimplePU(0, this, "fpsSuspendKey");
        this.__hideCursor = new ObservedPropertySimplePU(true, this, "hideCursor");
        this.__wheelMode = new ObservedPropertySimplePU(WheelMode.CLASSIC, this, "wheelMode");
        this.__statusText = new ObservedPropertySimplePU('等待无障碍扩展连接', this, "statusText");
        this.__selfTestText = new ObservedPropertySimplePU('尚未运行核心自检', this, "selfTestText");
        this.__bindRows = new ObservedPropertyObjectPU([], this, "bindRows");
        this.__bindKey = new ObservedPropertySimplePU(2017, this, "bindKey");
        this.__bindX = new ObservedPropertySimplePU(0.5, this, "bindX");
        this.__bindY = new ObservedPropertySimplePU(0.5, this, "bindY");
        this.__bindFpsOnly = new ObservedPropertySimplePU(false, this, "bindFpsOnly");
        this.setInitiallyProvidedValue(params);
        this.finalizeConstruction();
    }
    setInitiallyProvidedValue(params: Index_Params) {
        if (params.engine !== undefined) {
            this.engine = params.engine;
        }
        if (params.mappingEnabled !== undefined) {
            this.mappingEnabled = params.mappingEnabled;
        }
        if (params.fpsEnabled !== undefined) {
            this.fpsEnabled = params.fpsEnabled;
        }
        if (params.fpsToggleKey !== undefined) {
            this.fpsToggleKey = params.fpsToggleKey;
        }
        if (params.fpsSuspendKey !== undefined) {
            this.fpsSuspendKey = params.fpsSuspendKey;
        }
        if (params.hideCursor !== undefined) {
            this.hideCursor = params.hideCursor;
        }
        if (params.wheelMode !== undefined) {
            this.wheelMode = params.wheelMode;
        }
        if (params.statusText !== undefined) {
            this.statusText = params.statusText;
        }
        if (params.selfTestText !== undefined) {
            this.selfTestText = params.selfTestText;
        }
        if (params.bindRows !== undefined) {
            this.bindRows = params.bindRows;
        }
        if (params.bindKey !== undefined) {
            this.bindKey = params.bindKey;
        }
        if (params.bindX !== undefined) {
            this.bindX = params.bindX;
        }
        if (params.bindY !== undefined) {
            this.bindY = params.bindY;
        }
        if (params.bindFpsOnly !== undefined) {
            this.bindFpsOnly = params.bindFpsOnly;
        }
    }
    updateStateVars(params: Index_Params) {
    }
    purgeVariableDependenciesOnElmtId(rmElmtId) {
        this.__mappingEnabled.purgeDependencyOnElmtId(rmElmtId);
        this.__fpsEnabled.purgeDependencyOnElmtId(rmElmtId);
        this.__fpsToggleKey.purgeDependencyOnElmtId(rmElmtId);
        this.__fpsSuspendKey.purgeDependencyOnElmtId(rmElmtId);
        this.__hideCursor.purgeDependencyOnElmtId(rmElmtId);
        this.__wheelMode.purgeDependencyOnElmtId(rmElmtId);
        this.__statusText.purgeDependencyOnElmtId(rmElmtId);
        this.__selfTestText.purgeDependencyOnElmtId(rmElmtId);
        this.__bindRows.purgeDependencyOnElmtId(rmElmtId);
        this.__bindKey.purgeDependencyOnElmtId(rmElmtId);
        this.__bindX.purgeDependencyOnElmtId(rmElmtId);
        this.__bindY.purgeDependencyOnElmtId(rmElmtId);
        this.__bindFpsOnly.purgeDependencyOnElmtId(rmElmtId);
    }
    aboutToBeDeleted() {
        this.__mappingEnabled.aboutToBeDeleted();
        this.__fpsEnabled.aboutToBeDeleted();
        this.__fpsToggleKey.aboutToBeDeleted();
        this.__fpsSuspendKey.aboutToBeDeleted();
        this.__hideCursor.aboutToBeDeleted();
        this.__wheelMode.aboutToBeDeleted();
        this.__statusText.aboutToBeDeleted();
        this.__selfTestText.aboutToBeDeleted();
        this.__bindRows.aboutToBeDeleted();
        this.__bindKey.aboutToBeDeleted();
        this.__bindX.aboutToBeDeleted();
        this.__bindY.aboutToBeDeleted();
        this.__bindFpsOnly.aboutToBeDeleted();
        SubscriberManager.Get().delete(this.id__());
        this.aboutToBeDeletedInternal();
    }
    private engine;
    private __mappingEnabled: ObservedPropertySimplePU<boolean>;
    get mappingEnabled() {
        return this.__mappingEnabled.get();
    }
    set mappingEnabled(newValue: boolean) {
        this.__mappingEnabled.set(newValue);
    }
    private __fpsEnabled: ObservedPropertySimplePU<boolean>;
    get fpsEnabled() {
        return this.__fpsEnabled.get();
    }
    set fpsEnabled(newValue: boolean) {
        this.__fpsEnabled.set(newValue);
    }
    private __fpsToggleKey: ObservedPropertySimplePU<number>;
    get fpsToggleKey() {
        return this.__fpsToggleKey.get();
    }
    set fpsToggleKey(newValue: number) {
        this.__fpsToggleKey.set(newValue);
    }
    private __fpsSuspendKey: ObservedPropertySimplePU<number>;
    get fpsSuspendKey() {
        return this.__fpsSuspendKey.get();
    }
    set fpsSuspendKey(newValue: number) {
        this.__fpsSuspendKey.set(newValue);
    }
    private __hideCursor: ObservedPropertySimplePU<boolean>;
    get hideCursor() {
        return this.__hideCursor.get();
    }
    set hideCursor(newValue: boolean) {
        this.__hideCursor.set(newValue);
    }
    private __wheelMode: ObservedPropertySimplePU<WheelMode>;
    get wheelMode() {
        return this.__wheelMode.get();
    }
    set wheelMode(newValue: WheelMode) {
        this.__wheelMode.set(newValue);
    }
    private __statusText: ObservedPropertySimplePU<string>;
    get statusText() {
        return this.__statusText.get();
    }
    set statusText(newValue: string) {
        this.__statusText.set(newValue);
    }
    private __selfTestText: ObservedPropertySimplePU<string>;
    get selfTestText() {
        return this.__selfTestText.get();
    }
    set selfTestText(newValue: string) {
        this.__selfTestText.set(newValue);
    }
    private __bindRows: ObservedPropertyObjectPU<string[]>;
    get bindRows() {
        return this.__bindRows.get();
    }
    set bindRows(newValue: string[]) {
        this.__bindRows.set(newValue);
    }
    private __bindKey: ObservedPropertySimplePU<number>;
    get bindKey() {
        return this.__bindKey.get();
    }
    set bindKey(newValue: number) {
        this.__bindKey.set(newValue);
    }
    private __bindX: ObservedPropertySimplePU<number>;
    get bindX() {
        return this.__bindX.get();
    }
    set bindX(newValue: number) {
        this.__bindX.set(newValue);
    }
    private __bindY: ObservedPropertySimplePU<number>;
    get bindY() {
        return this.__bindY.get();
    }
    set bindY(newValue: number) {
        this.__bindY.set(newValue);
    }
    private __bindFpsOnly: ObservedPropertySimplePU<boolean>;
    get bindFpsOnly() {
        return this.__bindFpsOnly.get();
    }
    set bindFpsOnly(newValue: boolean) {
        this.__bindFpsOnly.set(newValue);
    }
    aboutToAppear(): void {
        this.syncState();
        this.runSelfTest();
        hilog.info(DOMAIN, 'Index', 'HarmonyOS branch page shown');
    }
    private syncState(): void {
        this.mappingEnabled = this.engine.isEnabled();
        this.fpsEnabled = this.engine.getProfile().fps.enabled;
        this.fpsToggleKey = this.engine.getProfile().fps.toggleKey;
        this.fpsSuspendKey = this.engine.getProfile().fps.suspendKey;
        this.hideCursor = this.engine.getProfile().fps.hideCursor;
        if (this.engine.getProfile().wheels.length > 0) {
            this.wheelMode = this.engine.getProfile().wheels[0].mode;
        }
        this.statusText = AccessibilityBridge.isAttached() ? '无障碍扩展已连接' : '等待在系统设置中启用无障碍服务';
        this.refreshBindRows();
    }
    private refreshBindRows(): void {
        const rows: string[] = [];
        const binds = this.engine.getProfile().binds;
        for (let index = 0; index < binds.length; index += 1) {
            const bind = binds[index];
            const fps = bind.fpsOnly ? ' · 仅 FPS' : '';
            rows.push(`${index + 1}. 键码 ${bind.key} → 点按 ${(bind.action.x * 100).toFixed(0)}%,${(bind.action.y * 100).toFixed(0)}%${fps}`);
        }
        this.bindRows = rows;
    }
    private addTapBinding(): void {
        this.engine.addTapBinding(this.bindKey, this.bindX, this.bindY, this.bindFpsOnly);
        ConfigStore.saveFrom(this.engine);
        this.refreshBindRows();
    }
    private removeBinding(index: number): void {
        this.engine.removeBinding(index);
        ConfigStore.saveFrom(this.engine);
        this.refreshBindRows();
    }
    private runSelfTest(): void {
        this.selfTestText = SelfTest.run();
    }
    initialRender() {
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Scroll.create();
            Scroll.width('100%');
            Scroll.height('100%');
            Scroll.backgroundColor('#10131A');
        }, Scroll);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Column.create({ space: 14 });
            Column.width('100%');
            Column.padding(18);
        }, Column);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create();
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Column.create({ space: 4 });
            Column.alignItems(HorizontalAlign.Start);
            Column.layoutWeight(1);
        }, Column);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('scrcpy-pad');
            Text.fontSize(28);
            Text.fontWeight(FontWeight.Bold);
            Text.fontColor('#F4F7FB');
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('HarmonyOS 6+ 原生迁移分支');
            Text.fontSize(14);
            Text.fontColor('#9FA9BA');
        }, Text);
        Text.pop();
        Column.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Button.createWithLabel('刷新');
            Button.fontSize(14);
            Button.backgroundColor('#283244');
            Button.fontColor('#DCE5F2');
            Button.onClick(() => {
                this.syncState();
            });
        }, Button);
        Button.pop();
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Column.create({ space: 8 });
            Column.padding(14);
            Column.backgroundColor('#171D28');
            Column.borderRadius(12);
            Column.width('100%');
        }, Column);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create();
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('无障碍服务');
            Text.fontColor('#DCE5F2');
            Text.layoutWeight(1);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create(AccessibilityBridge.isAttached() ? '已连接' : '未连接');
            Text.fontColor(AccessibilityBridge.isAttached() ? '#4ADE80' : '#F87171');
            Text.fontWeight(FontWeight.Medium);
        }, Text);
        Text.pop();
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('首次使用请在“设置 > 辅助功能”中启用“scrcpy-pad 键位引擎”。');
            Text.fontSize(13);
            Text.fontColor('#9FA9BA');
            Text.width('100%');
        }, Text);
        Text.pop();
        Column.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Column.create({ space: 12 });
            Column.padding(14);
            Column.backgroundColor('#171D28');
            Column.borderRadius(12);
            Column.width('100%');
        }, Column);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create();
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('映射总开关');
            Text.fontColor('#F4F7FB');
            Text.layoutWeight(1);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Toggle.create({ type: ToggleType.Switch, isOn: this.mappingEnabled });
            Toggle.selectedColor('#38BDF8');
            Toggle.onChange((value: boolean) => {
                this.mappingEnabled = value;
                this.engine.setEnabled(value);
                ConfigStore.saveFrom(this.engine);
            });
        }, Toggle);
        Toggle.pop();
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('默认按键 F8；FPS 模式不依赖该开关。');
            Text.fontSize(12);
            Text.fontColor('#8994A7');
            Text.width('100%');
        }, Text);
        Text.pop();
        Column.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Column.create({ space: 12 });
            Column.padding(14);
            Column.backgroundColor('#171D28');
            Column.borderRadius(12);
            Column.width('100%');
        }, Column);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('轮盘方向模式');
            Text.fontColor('#F4F7FB');
            Text.fontWeight(FontWeight.Medium);
            Text.width('100%');
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create({ space: 10 });
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Button.createWithLabel('经典（标准）');
            Button.layoutWeight(1);
            Button.backgroundColor(this.wheelMode === WheelMode.CLASSIC ? '#0EA5E9' : '#283244');
            Button.onClick(() => {
                this.wheelMode = WheelMode.CLASSIC;
                this.engine.setAllWheelModes(WheelMode.CLASSIC);
                ConfigStore.saveFrom(this.engine);
            });
        }, Button);
        Button.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Button.createWithLabel('灵敏（后按覆盖）');
            Button.layoutWeight(1);
            Button.backgroundColor(this.wheelMode === WheelMode.SENSITIVE ? '#0EA5E9' : '#283244');
            Button.onClick(() => {
                this.wheelMode = WheelMode.SENSITIVE;
                this.engine.setAllWheelModes(WheelMode.SENSITIVE);
                ConfigStore.saveFrom(this.engine);
            });
        }, Button);
        Button.pop();
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('灵敏模式按轴独立记录后按方向；左右与上下互不干扰。');
            Text.fontSize(12);
            Text.fontColor('#8994A7');
            Text.width('100%');
        }, Text);
        Text.pop();
        Column.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Column.create({ space: 12 });
            Column.padding(14);
            Column.backgroundColor('#171D28');
            Column.borderRadius(12);
            Column.width('100%');
        }, Column);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create();
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('鼠标瞄准（FPS）');
            Text.fontColor('#F4F7FB');
            Text.layoutWeight(1);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Toggle.create({ type: ToggleType.Switch, isOn: this.fpsEnabled });
            Toggle.selectedColor('#38BDF8');
            Toggle.onChange((value: boolean) => {
                this.fpsEnabled = value;
                this.engine.setFpsEnabled(value);
                ConfigStore.saveFrom(this.engine);
            });
        }, Toggle);
        Toggle.pop();
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('FPS 开关独立于映射总开关；进入 FPS 不自动隐藏指针。');
            Text.fontSize(12);
            Text.fontColor('#8994A7');
            Text.width('100%');
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create({ space: 8 });
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('FPS 开关键码');
            Text.fontColor('#DCE5F2');
            Text.layoutWeight(1);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            TextInput.create({ text: this.fpsToggleKey.toString() });
            TextInput.type(InputType.Number);
            TextInput.width(110);
            TextInput.height(38);
            TextInput.backgroundColor('#0E131C');
            TextInput.fontColor('#F4F7FB');
            TextInput.onChange((value: string) => {
                const parsed = Number.parseInt(value);
                if (!Number.isNaN(parsed)) {
                    this.fpsToggleKey = parsed;
                    this.engine.setFpsToggleKey(parsed);
                    ConfigStore.saveFrom(this.engine);
                }
            });
        }, TextInput);
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create({ space: 8 });
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('临时退出键码');
            Text.fontColor('#DCE5F2');
            Text.layoutWeight(1);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            TextInput.create({ text: this.fpsSuspendKey.toString() });
            TextInput.type(InputType.Number);
            TextInput.width(110);
            TextInput.height(38);
            TextInput.backgroundColor('#0E131C');
            TextInput.fontColor('#F4F7FB');
            TextInput.onChange((value: string) => {
                const parsed = Number.parseInt(value);
                if (!Number.isNaN(parsed)) {
                    this.fpsSuspendKey = parsed;
                    this.engine.setFpsSuspendKey(parsed);
                    ConfigStore.saveFrom(this.engine);
                }
            });
        }, TextInput);
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create();
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('进入 FPS 时隐藏指针（配置项）');
            Text.fontColor('#DCE5F2');
            Text.layoutWeight(1);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Toggle.create({ type: ToggleType.Switch, isOn: this.hideCursor });
            Toggle.selectedColor('#38BDF8');
            Toggle.onChange((value: boolean) => {
                this.hideCursor = value;
                this.engine.setFpsHideCursor(value);
                ConfigStore.saveFrom(this.engine);
            });
        }, Toggle);
        Toggle.pop();
        Row.pop();
        Column.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Column.create({ space: 12 });
            Column.padding(14);
            Column.backgroundColor('#171D28');
            Column.borderRadius(12);
            Column.width('100%');
        }, Column);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('点按键位');
            Text.fontColor('#F4F7FB');
            Text.fontWeight(FontWeight.Medium);
            Text.width('100%');
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create({ space: 8 });
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('键码');
            Text.fontColor('#DCE5F2');
            Text.width(48);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            TextInput.create({ text: this.bindKey.toString() });
            TextInput.type(InputType.Number);
            TextInput.layoutWeight(1);
            TextInput.height(38);
            TextInput.backgroundColor('#0E131C');
            TextInput.fontColor('#F4F7FB');
            TextInput.onChange((value: string) => {
                const parsed = Number.parseInt(value);
                if (!Number.isNaN(parsed)) {
                    this.bindKey = parsed;
                }
            });
        }, TextInput);
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create({ space: 8 });
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('X%');
            Text.fontColor('#DCE5F2');
            Text.width(48);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            TextInput.create({ text: (this.bindX * 100).toFixed(0) });
            TextInput.type(InputType.Number);
            TextInput.layoutWeight(1);
            TextInput.height(38);
            TextInput.backgroundColor('#0E131C');
            TextInput.fontColor('#F4F7FB');
            TextInput.onChange((value: string) => {
                const parsed = Number.parseFloat(value);
                if (!Number.isNaN(parsed)) {
                    this.bindX = Math.max(0, Math.min(100, parsed)) / 100;
                }
            });
        }, TextInput);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('Y%');
            Text.fontColor('#DCE5F2');
            Text.width(48);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            TextInput.create({ text: (this.bindY * 100).toFixed(0) });
            TextInput.type(InputType.Number);
            TextInput.layoutWeight(1);
            TextInput.height(38);
            TextInput.backgroundColor('#0E131C');
            TextInput.fontColor('#F4F7FB');
            TextInput.onChange((value: string) => {
                const parsed = Number.parseFloat(value);
                if (!Number.isNaN(parsed)) {
                    this.bindY = Math.max(0, Math.min(100, parsed)) / 100;
                }
            });
        }, TextInput);
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create();
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('仅 FPS 模式生效');
            Text.fontColor('#DCE5F2');
            Text.layoutWeight(1);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Toggle.create({ type: ToggleType.Switch, isOn: this.bindFpsOnly });
            Toggle.selectedColor('#38BDF8');
            Toggle.onChange((value: boolean) => {
                this.bindFpsOnly = value;
            });
        }, Toggle);
        Toggle.pop();
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Button.createWithLabel('新增点按绑定');
            Button.width('100%');
            Button.backgroundColor('#0EA5E9');
            Button.onClick(() => {
                this.addTapBinding();
            });
        }, Button);
        Button.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            If.create();
            if (this.bindRows.length === 0) {
                this.ifElseBranchUpdateFunction(0, () => {
                    this.observeComponentCreation2((elmtId, isInitialRender) => {
                        Text.create('暂无绑定');
                        Text.fontSize(13);
                        Text.fontColor('#8994A7');
                        Text.width('100%');
                    }, Text);
                    Text.pop();
                });
            }
            else {
                this.ifElseBranchUpdateFunction(1, () => {
                    this.observeComponentCreation2((elmtId, isInitialRender) => {
                        ForEach.create();
                        const forEachItemGenFunction = (_item, index: number) => {
                            const row = _item;
                            this.observeComponentCreation2((elmtId, isInitialRender) => {
                                Row.create({ space: 8 });
                                Row.width('100%');
                            }, Row);
                            this.observeComponentCreation2((elmtId, isInitialRender) => {
                                Text.create(row);
                                Text.fontSize(13);
                                Text.fontColor('#DCE5F2');
                                Text.layoutWeight(1);
                            }, Text);
                            Text.pop();
                            this.observeComponentCreation2((elmtId, isInitialRender) => {
                                Button.createWithLabel('删除');
                                Button.fontSize(12);
                                Button.height(32);
                                Button.backgroundColor('#7F1D1D');
                                Button.onClick(() => {
                                    this.removeBinding(index);
                                });
                            }, Button);
                            Button.pop();
                            Row.pop();
                        };
                        this.forEachUpdateFunction(elmtId, this.bindRows, forEachItemGenFunction, (row: string, index: number) => `${index}:${row}`, true, true);
                    }, ForEach);
                    ForEach.pop();
                });
            }
        }, If);
        If.pop();
        Column.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Column.create({ space: 10 });
            Column.padding(14);
            Column.backgroundColor('#171D28');
            Column.borderRadius(12);
            Column.width('100%');
        }, Column);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Row.create();
            Row.width('100%');
        }, Row);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('核心自检');
            Text.fontColor('#F4F7FB');
            Text.fontWeight(FontWeight.Medium);
            Text.layoutWeight(1);
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Button.createWithLabel('运行');
            Button.fontSize(14);
            Button.backgroundColor('#0EA5E9');
            Button.onClick(() => {
                this.runSelfTest();
            });
        }, Button);
        Button.pop();
        Row.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create(this.selfTestText);
            Text.fontSize(13);
            Text.fontColor('#7DD3FC');
            Text.width('100%');
        }, Text);
        Text.pop();
        Column.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Column.create({ space: 8 });
            Column.padding(14);
            Column.backgroundColor('#201B13');
            Column.borderRadius(12);
            Column.width('100%');
        }, Column);
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('适配状态');
            Text.fontColor('#F4F7FB');
            Text.fontWeight(FontWeight.Medium);
            Text.width('100%');
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create(this.statusText);
            Text.fontColor('#7DD3FC');
            Text.fontSize(13);
            Text.width('100%');
        }, Text);
        Text.pop();
        this.observeComponentCreation2((elmtId, isInitialRender) => {
            Text.create('HarmonyOS 普通应用只允许无障碍扩展注入单指手势；无限时长长按与真正多指注入受系统权限限制。');
            Text.fontSize(12);
            Text.fontColor('#FBBF24');
            Text.width('100%');
        }, Text);
        Text.pop();
        Column.pop();
        Column.pop();
        Scroll.pop();
    }
    rerender() {
        this.updateDirtyElements();
    }
    static getEntryName(): string {
        return "Index";
    }
}
registerNamedRoute(() => new Index(undefined, {}), "", { bundleName: "com.azrl.scrcpypad", moduleName: "entry", pagePath: "pages/Index", pageFullPath: "entry/src/main/ets/pages/Index", integratedHsp: "false", moduleType: "followWithHap" });
