<script lang="ts">
  import mark from "./assets/urma-node.svg?raw";
  import type { Phase } from "./progress";

  let { phase }: { phase: Phase } = $props();

  const duration = 4.2;
  const travelStart = 0.06;
  const travelEnd = 0.94;
  const still = matchMedia("(prefers-reduced-motion: reduce)");

  let host: HTMLSpanElement;
  let shown = 0;

  const goal = $derived.by(() => {
    if (phase === "failed") return 0;
    if (phase === "connecting") return travelStart;
    if (phase.complete) return 1;
    return travelStart + (travelEnd - travelStart) * phase.fraction;
  });

  $effect(() => {
    const target = goal;
    const svg = host.querySelector("svg")!;
    svg.pauseAnimations();
    if (target <= shown || still.matches) {
      shown = target;
      svg.setCurrentTime(shown * duration);
      return;
    }
    let last = performance.now();
    let frame = requestAnimationFrame(function step(now) {
      shown = Math.min(target, shown + (now - last) / 1000 / duration);
      last = now;
      svg.setCurrentTime(shown * duration);
      if (shown < target) frame = requestAnimationFrame(step);
    });
    return () => cancelAnimationFrame(frame);
  });
</script>

<span class="logo" bind:this={host}>{@html mark}</span>
