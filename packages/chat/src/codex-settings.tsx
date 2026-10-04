/**
 * Codex の入力設定（VP design 74 の UI 共有、GFP design 11）。
 *
 * モード（通常 / Plan）・実効権限の詳細・model / effort の選択・保存中 / エラー表示を共通にする。
 * **権限の選択欄はアプリが `permission` slot で差し込む**（VP = Codex 本体の権限メニュー、
 * GFP = 会話が持つ 4 段階）。transport・store は持たず、値と操作を props で受ける。
 *
 * 選択は確定値（host の `codex_config`）が届くまで画面に反映しない（`changeCodexSelection`）。
 */
import { For, Show, type JSX } from 'solid-js'
import type { CodexConfigView } from './generated/CodexConfigView'
import type { CodexRuntime } from './generated/CodexRuntime'

export type CodexSettingsProps = {
  config: CodexConfigView | null | undefined
  /** 応答中・送信待ちがあるなど、設定を変えられない状態。 */
  busy: boolean
  /** host と接続していて native の状態を読める。 */
  connected: boolean
  /** model / effort の保存結果待ち。 */
  saving: boolean
  error: string | null | undefined
  onSelection: (model: string, effort: string) => void
  onMode: (mode: string) => void
  /** transport がモード変更を実装していない場合は操作不可にする。 */
  modeAvailable?: boolean
  /**
   * host が native の実効権限（`config.runtime`）を提供するか（既定 true）。false のときは「確認中」を出さず、
   * 実効権限の表示をアプリに任せる（GFP は turn ごとに権限を明示するので native の設定値は実効値にならない）。
   */
  runtimeAvailable?: boolean
  /** 権限の選択欄（アプリが所有する）。 */
  permission?: JSX.Element
}

/** ブラウザ上の仮選択を、保存結果が届くまで確定値へ戻す。 */
export function changeCodexSelection(select: HTMLSelectElement, confirmed: string, submit: (value: string) => void): void {
  const requested = select.value
  select.value = confirmed
  submit(requested)
}

/** 次の送信に使う model（明示選択 → native の既定）。 */
export function codexModelOf(config: CodexConfigView | null | undefined): string {
  return config?.selection?.model ?? config?.model ?? ''
}

/** 次の送信に使う effort（明示選択 → native の既定）。 */
export function codexEffortOf(config: CodexConfigView | null | undefined): string {
  return config?.selection?.effort ?? config?.effort ?? ''
}

/** モード選択と実効権限の詳細。権限欄は slot。 */
export function CodexRuntimeControls(props: {
  runtime: CodexRuntime | null | undefined
  busy: boolean
  connected: boolean
  changeMode: (mode: string) => void
  modeAvailable?: boolean
  runtimeAvailable?: boolean
  permission?: JSX.Element
}) {
  return <>
    <select class="conversation-model-select" aria-label="Codex mode"
      title="次の応答の進め方。Plan は計画、通常は実行。権限の設定は変更しません。"
      disabled={props.busy || !props.connected || props.modeAvailable === false}
      onChange={event => {
        const selected = event.currentTarget.value
        event.currentTarget.value = props.runtime?.mode ?? ''
        if (!props.busy && props.connected && props.modeAvailable !== false) props.changeMode(selected)
      }}>
      <Show when={!['plan', 'default'].includes(props.runtime?.mode ?? '')}>
        <option value="" selected disabled>モード未確認</option>
      </Show>
      <option value="default" selected={props.runtime?.mode === 'default'}>通常</option>
      <option value="plan" selected={props.runtime?.mode === 'plan'}>Plan</option>
    </select>
    {props.permission}
    <Show when={props.runtimeAvailable !== false}>
    <Show when={props.runtime} fallback={<span class="conversation-model-readonly">実効権限を確認中…</span>}>
      {runtime => <details class="conversation-model-readonly">
        <summary>実効権限{props.connected ? '' : '（最終確認）'}: {runtime().approval} / {runtime().sandbox}</summary>
        <div>承認先: {runtime().reviewer ?? '未確認'}</div>
        <div>ネットワーク: {runtime().network_access == null ? '未確認' : runtime().network_access ? '許可' : '制限あり'}</div>
        <Show when={runtime().profile}><div>プロファイル: {runtime().profile}</div></Show>
        <Show when={runtime().writable_roots.length}>
          <div>書き込み可能な場所:</div>
          <For each={runtime().writable_roots}>{path => <div>{path}</div>}</For>
        </Show>
      </details>}
    </Show>
    </Show>
  </>
}

/** Codex の入力設定一式。 */
export function CodexSettingsControls(props: CodexSettingsProps) {
  const model = () => codexModelOf(props.config)
  const effort = () => codexEffortOf(props.config)
  const models = () => props.config?.models ?? []
  const current = () => models().find(m => m.model === model())
  return <>
    <CodexRuntimeControls runtime={props.config?.runtime} busy={props.busy || !model()}
      connected={props.connected} modeAvailable={props.modeAvailable} runtimeAvailable={props.runtimeAvailable}
      changeMode={props.onMode} permission={props.permission} />
    <Show when={models().length > 0} fallback={<span class="conversation-model-readonly">{props.config?.error ?? 'モデル候補を取得中…'}</span>}>
      <select class="conversation-model-select" aria-label="Codex model" title="次の送信に使うモデル" disabled={props.busy}
        onChange={e => changeCodexSelection(e.currentTarget, model(), value => {
          const chosen = models().find(m => m.model === value)
          if (chosen) props.onSelection(chosen.model, chosen.default_effort)
        })}>
        <Show when={!current()}>
          <option value={model()} selected disabled>{model() || 'モデルを選択'}</option>
        </Show>
        <For each={models()}>{m => <option value={m.model} selected={m.model === model()}>{m.label}</option>}</For>
      </select>
      <select class="conversation-model-select" aria-label="Codex effort" title="次の送信の reasoning effort" disabled={props.busy || !current()}
        onChange={e => changeCodexSelection(e.currentTarget, effort(), value => props.onSelection(model(), value))}>
        <Show when={!current()?.efforts.includes(effort())}>
          <option value={effort()} selected disabled>{effort() || 'Codex 既定'}</option>
        </Show>
        <For each={current()?.efforts ?? []}>{e => <option value={e} selected={e === effort()}>{e}</option>}</For>
      </select>
    </Show>
    <Show when={props.saving}><span class="conversation-model-readonly">保存中…</span></Show>
    <Show when={props.error || (models().length > 0 && props.config?.error)}>
      <span role="status" class="conversation-model-readonly">{props.error || props.config?.error}</span>
    </Show>
  </>
}
