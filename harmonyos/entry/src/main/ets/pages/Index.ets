import { hilog } from '@kit.PerformanceAnalysisKit';
import { AccessibilityBridge } from '../service/AccessibilityBridge';
import { WheelMode } from '../model/Keymap';
import { SelfTest } from '../engine/SelfTest';
import { ConfigStore } from '../service/ConfigStore';

const DOMAIN = 0x5343;

@Entry
@Component
struct Index {
  private engine = AccessibilityBridge.getEngine();
  @State mappingEnabled: boolean = false;
  @State fpsEnabled: boolean = false;
  @State fpsToggleKey: number = 0;
  @State fpsSuspendKey: number = 0;
  @State hideCursor: boolean = true;
  @State wheelMode: WheelMode = WheelMode.CLASSIC;
  @State statusText: string = '等待无障碍扩展连接';
  @State selfTestText: string = '尚未运行核心自检';
  @State bindRows: string[] = [];
  @State bindKey: number = 2017;
  @State bindX: number = 0.5;
  @State bindY: number = 0.5;
  @State bindFpsOnly: boolean = false;

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

  build() {
    Scroll() {
      Column({ space: 14 }) {
        Row() {
          Column({ space: 4 }) {
            Text('scrcpy-pad')
              .fontSize(28)
              .fontWeight(FontWeight.Bold)
              .fontColor('#F4F7FB')
            Text('HarmonyOS 6+ 原生迁移分支')
              .fontSize(14)
              .fontColor('#9FA9BA')
          }
          .alignItems(HorizontalAlign.Start)
          .layoutWeight(1)
          Button('刷新')
            .fontSize(14)
            .backgroundColor('#283244')
            .fontColor('#DCE5F2')
            .onClick(() => {
              this.syncState();
            })
        }
        .width('100%')

        Column({ space: 8 }) {
          Row() {
            Text('无障碍服务')
              .fontColor('#DCE5F2')
              .layoutWeight(1)
            Text(AccessibilityBridge.isAttached() ? '已连接' : '未连接')
              .fontColor(AccessibilityBridge.isAttached() ? '#4ADE80' : '#F87171')
              .fontWeight(FontWeight.Medium)
          }
          .width('100%')
          Text('首次使用请在“设置 > 辅助功能”中启用“scrcpy-pad 键位引擎”。')
            .fontSize(13)
            .fontColor('#9FA9BA')
            .width('100%')
        }
        .padding(14)
        .backgroundColor('#171D28')
        .borderRadius(12)
        .width('100%')

        Column({ space: 12 }) {
          Row() {
            Text('映射总开关')
              .fontColor('#F4F7FB')
              .layoutWeight(1)
            Toggle({ type: ToggleType.Switch, isOn: this.mappingEnabled })
              .selectedColor('#38BDF8')
              .onChange((value: boolean) => {
                this.mappingEnabled = value;
                this.engine.setEnabled(value);
        ConfigStore.saveFrom(this.engine);
              })
          }
          .width('100%')
          Text('默认按键 F8；FPS 模式不依赖该开关。')
            .fontSize(12)
            .fontColor('#8994A7')
            .width('100%')
        }
        .padding(14)
        .backgroundColor('#171D28')
        .borderRadius(12)
        .width('100%')

        Column({ space: 12 }) {
          Text('轮盘方向模式')
            .fontColor('#F4F7FB')
            .fontWeight(FontWeight.Medium)
            .width('100%')
          Row({ space: 10 }) {
            Button('经典（标准）')
              .layoutWeight(1)
              .backgroundColor(this.wheelMode === WheelMode.CLASSIC ? '#0EA5E9' : '#283244')
              .onClick(() => {
                this.wheelMode = WheelMode.CLASSIC;
                this.engine.setAllWheelModes(WheelMode.CLASSIC);
                ConfigStore.saveFrom(this.engine);
              })
            Button('灵敏（后按覆盖）')
              .layoutWeight(1)
              .backgroundColor(this.wheelMode === WheelMode.SENSITIVE ? '#0EA5E9' : '#283244')
              .onClick(() => {
                this.wheelMode = WheelMode.SENSITIVE;
                this.engine.setAllWheelModes(WheelMode.SENSITIVE);
                ConfigStore.saveFrom(this.engine);
              })
          }
          .width('100%')
          Text('灵敏模式按轴独立记录后按方向；左右与上下互不干扰。')
            .fontSize(12)
            .fontColor('#8994A7')
            .width('100%')
        }
        .padding(14)
        .backgroundColor('#171D28')
        .borderRadius(12)
        .width('100%')

        Column({ space: 12 }) {
          Row() {
            Text('鼠标瞄准（FPS）')
              .fontColor('#F4F7FB')
              .layoutWeight(1)
            Toggle({ type: ToggleType.Switch, isOn: this.fpsEnabled })
              .selectedColor('#38BDF8')
              .onChange((value: boolean) => {
                this.fpsEnabled = value;
                this.engine.setFpsEnabled(value);
                ConfigStore.saveFrom(this.engine);
              })
          }
          .width('100%')
          Text('FPS 开关独立于映射总开关；进入 FPS 不自动隐藏指针。')
            .fontSize(12)
            .fontColor('#8994A7')
            .width('100%')
          Row({ space: 8 }) {
            Text('FPS 开关键码')
              .fontColor('#DCE5F2')
              .layoutWeight(1)
            TextInput({ text: this.fpsToggleKey.toString() })
              .type(InputType.Number)
              .width(110)
              .height(38)
              .backgroundColor('#0E131C')
              .fontColor('#F4F7FB')
              .onChange((value: string) => {
                const parsed = Number.parseInt(value);
                if (!Number.isNaN(parsed)) {
                  this.fpsToggleKey = parsed;
                  this.engine.setFpsToggleKey(parsed);
                  ConfigStore.saveFrom(this.engine);
                }
              })
          }
          .width('100%')
          Row({ space: 8 }) {
            Text('临时退出键码')
              .fontColor('#DCE5F2')
              .layoutWeight(1)
            TextInput({ text: this.fpsSuspendKey.toString() })
              .type(InputType.Number)
              .width(110)
              .height(38)
              .backgroundColor('#0E131C')
              .fontColor('#F4F7FB')
              .onChange((value: string) => {
                const parsed = Number.parseInt(value);
                if (!Number.isNaN(parsed)) {
                  this.fpsSuspendKey = parsed;
                  this.engine.setFpsSuspendKey(parsed);
                  ConfigStore.saveFrom(this.engine);
                }
              })
          }
          .width('100%')
          Row() {
            Text('进入 FPS 时隐藏指针（配置项）')
              .fontColor('#DCE5F2')
              .layoutWeight(1)
            Toggle({ type: ToggleType.Switch, isOn: this.hideCursor })
              .selectedColor('#38BDF8')
              .onChange((value: boolean) => {
                this.hideCursor = value;
                this.engine.setFpsHideCursor(value);
                ConfigStore.saveFrom(this.engine);
              })
          }
          .width('100%')
        }
        .padding(14)
        .backgroundColor('#171D28')
        .borderRadius(12)
        .width('100%')

        Column({ space: 12 }) {
          Text('点按键位')
            .fontColor('#F4F7FB')
            .fontWeight(FontWeight.Medium)
            .width('100%')
          Row({ space: 8 }) {
            Text('键码')
              .fontColor('#DCE5F2')
              .width(48)
            TextInput({ text: this.bindKey.toString() })
              .type(InputType.Number)
              .layoutWeight(1)
              .height(38)
              .backgroundColor('#0E131C')
              .fontColor('#F4F7FB')
              .onChange((value: string) => {
                const parsed = Number.parseInt(value);
                if (!Number.isNaN(parsed)) {
                  this.bindKey = parsed;
                }
              })
          }
          .width('100%')
          Row({ space: 8 }) {
            Text('X%')
              .fontColor('#DCE5F2')
              .width(48)
            TextInput({ text: (this.bindX * 100).toFixed(0) })
              .type(InputType.Number)
              .layoutWeight(1)
              .height(38)
              .backgroundColor('#0E131C')
              .fontColor('#F4F7FB')
              .onChange((value: string) => {
                const parsed = Number.parseFloat(value);
                if (!Number.isNaN(parsed)) {
                  this.bindX = Math.max(0, Math.min(100, parsed)) / 100;
                }
              })
            Text('Y%')
              .fontColor('#DCE5F2')
              .width(48)
            TextInput({ text: (this.bindY * 100).toFixed(0) })
              .type(InputType.Number)
              .layoutWeight(1)
              .height(38)
              .backgroundColor('#0E131C')
              .fontColor('#F4F7FB')
              .onChange((value: string) => {
                const parsed = Number.parseFloat(value);
                if (!Number.isNaN(parsed)) {
                  this.bindY = Math.max(0, Math.min(100, parsed)) / 100;
                }
              })
          }
          .width('100%')
          Row() {
            Text('仅 FPS 模式生效')
              .fontColor('#DCE5F2')
              .layoutWeight(1)
            Toggle({ type: ToggleType.Switch, isOn: this.bindFpsOnly })
              .selectedColor('#38BDF8')
              .onChange((value: boolean) => {
                this.bindFpsOnly = value;
              })
          }
          .width('100%')
          Button('新增点按绑定')
            .width('100%')
            .backgroundColor('#0EA5E9')
            .onClick(() => {
              this.addTapBinding();
            })
          if (this.bindRows.length === 0) {
            Text('暂无绑定')
              .fontSize(13)
              .fontColor('#8994A7')
              .width('100%')
          } else {
            ForEach(this.bindRows, (row: string, index: number) => {
              Row({ space: 8 }) {
                Text(row)
                  .fontSize(13)
                  .fontColor('#DCE5F2')
                  .layoutWeight(1)
                Button('删除')
                  .fontSize(12)
                  .height(32)
                  .backgroundColor('#7F1D1D')
                  .onClick(() => {
                    this.removeBinding(index);
                  })
              }
              .width('100%')
            }, (row: string, index: number) => `${index}:${row}`)
          }
        }
        .padding(14)
        .backgroundColor('#171D28')
        .borderRadius(12)
        .width('100%')

        Column({ space: 10 }) {
          Row() {
            Text('核心自检')
              .fontColor('#F4F7FB')
              .fontWeight(FontWeight.Medium)
              .layoutWeight(1)
            Button('运行')
              .fontSize(14)
              .backgroundColor('#0EA5E9')
              .onClick(() => {
                this.runSelfTest();
              })
          }
          .width('100%')
          Text(this.selfTestText)
            .fontSize(13)
            .fontColor('#7DD3FC')
            .width('100%')
        }
        .padding(14)
        .backgroundColor('#171D28')
        .borderRadius(12)
        .width('100%')

        Column({ space: 8 }) {
          Text('适配状态')
            .fontColor('#F4F7FB')
            .fontWeight(FontWeight.Medium)
            .width('100%')
          Text(this.statusText)
            .fontColor('#7DD3FC')
            .fontSize(13)
            .width('100%')
          Text('HarmonyOS 普通应用只允许无障碍扩展注入单指手势；无限时长长按与真正多指注入受系统权限限制。')
            .fontSize(12)
            .fontColor('#FBBF24')
            .width('100%')
        }
        .padding(14)
        .backgroundColor('#201B13')
        .borderRadius(12)
        .width('100%')
      }
      .width('100%')
      .padding(18)
    }
    .width('100%')
    .height('100%')
    .backgroundColor('#10131A')
  }
}
