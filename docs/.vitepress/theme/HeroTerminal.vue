<script setup lang="ts">
const lines = [
  { kind: "cmd", text: "onelf bundle-libs ./app --from-binary /usr/bin/app" },
  { kind: "out", text: "Copied 52 libraries (177.7 MB) to ./app/lib" },
  { kind: "out", text: "Injected AT_EXECFN bootstrap into 1 binaries" },
  { kind: "cmd", text: "onelf pack ./app -o app.onelf --command bin/app" },
  { kind: "out", text: "Payload: 63.0 MB (zstd level 12)" },
  { kind: "out", text: "Output:  79.8 MB" },
  { kind: "cmd", text: "./app.onelf" },
  { kind: "ok", text: "mounted privately, running, nothing left behind" },
];
</script>

<template>
  <div class="hero-terminal">
    <div class="window">
      <div class="titlebar">
        <span class="dot"></span><span class="dot"></span><span class="dot"></span>
        <span class="title">app.onelf</span>
      </div>
      <pre class="screen"><template v-for="(line, i) in lines" :key="i"><span :class="['line', line.kind]" :style="{ animationDelay: `${i * 0.18}s` }"><span v-if="line.kind === 'cmd'" class="prompt">$ </span>{{ line.text }}</span>
</template><span class="cursor" :style="{ animationDelay: `${lines.length * 0.18}s` }"></span></pre>
    </div>
    <div class="layout">
      <span class="block rt">runtime</span>
      <span class="block manifest">manifest</span>
      <span class="block payload">payload</span>
      <span class="block footer">footer</span>
      <span class="caption">one file, in that order</span>
    </div>
  </div>
</template>

<style scoped>
.hero-terminal {
  width: 100%;
  max-width: 560px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.window {
  border: 1px solid var(--vp-c-divider);
  border-radius: 12px;
  background: var(--onelf-terminal-bg);
  box-shadow:
    0 30px 60px -30px rgba(0, 0, 0, 0.6),
    0 0 0 1px rgba(255, 255, 255, 0.02) inset;
  overflow: hidden;
}

.titlebar {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 10px 14px;
  border-bottom: 1px solid var(--vp-c-divider);
  background: var(--onelf-terminal-bar);
}

.dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: var(--vp-c-divider);
}
.dot:first-child {
  background: var(--vp-c-brand-1);
}

.title {
  margin-left: auto;
  font-family: var(--vp-font-family-mono);
  font-size: 12px;
  color: var(--vp-c-text-3);
}

.screen {
  margin: 0;
  padding: 16px 18px 14px;
  font-family: var(--vp-font-family-mono);
  font-size: 13px;
  line-height: 1.7;
  color: var(--vp-c-text-2);
  white-space: pre-wrap;
  word-break: break-word;
  min-height: 236px;
}

.line {
  display: block;
  opacity: 0;
  animation: rise 0.35s ease-out forwards;
}
.line.cmd {
  color: var(--vp-c-text-1);
}
.line.ok {
  color: var(--vp-c-brand-1);
}
.prompt {
  color: var(--vp-c-brand-1);
}

.cursor {
  display: inline-block;
  width: 8px;
  height: 15px;
  vertical-align: -2px;
  background: var(--vp-c-brand-1);
  opacity: 0;
  animation: rise 0.2s ease-out forwards, blink 1s steps(2, start) infinite;
}

.layout {
  display: grid;
  grid-template-columns: 1.2fr 1fr 2.2fr 0.7fr;
  gap: 4px;
  align-items: stretch;
  font-family: var(--vp-font-family-mono);
  font-size: 11px;
  letter-spacing: 0.04em;
  text-transform: uppercase;
}
.block {
  padding: 7px 0;
  text-align: center;
  border-radius: 6px;
  border: 1px solid var(--vp-c-divider);
  color: var(--vp-c-text-2);
  background: var(--vp-c-bg-soft);
}
.block.payload {
  border-color: color-mix(in srgb, var(--vp-c-brand-1) 45%, transparent);
  color: var(--vp-c-brand-1);
  background: var(--vp-c-brand-soft);
}
.caption {
  grid-column: 1 / -1;
  text-align: right;
  color: var(--vp-c-text-3);
  text-transform: none;
  letter-spacing: 0;
  font-size: 12px;
}

@keyframes rise {
  from {
    opacity: 0;
    transform: translateY(4px);
  }
  to {
    opacity: 1;
    transform: none;
  }
}
@keyframes blink {
  to {
    visibility: hidden;
  }
}

@media (prefers-reduced-motion: reduce) {
  .line,
  .cursor {
    animation: none;
    opacity: 1;
  }
}
</style>
