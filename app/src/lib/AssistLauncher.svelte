<script lang="ts">
  // The button that opens the AI assist panel, shown in a transcript's header; `on` while it's open.
  let { on, label, title, onclick }: { on: boolean; label: string; title: string; onclick: () => void } = $props();
</script>

<button class="assist-launch" class:on {onclick} {title}>
  <span class="spark">✦</span> {label}
</button>

<style>
  /* The "smart" AI Assist launcher — a vibrant, gently shimmering gradient pill that appears top-right
     once a live session starts, so it reads as intelligent and is reachable the moment you press Start. */
  .assist-launch {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    font-family: inherit;
    font-size: 13px;
    font-weight: 600;
    color: #fff;
    background: linear-gradient(120deg, #8b5cf6, #ec4899, #f97316, #ec4899, #8b5cf6);
    background-size: 240% 100%;
    border: none;
    border-radius: 999px;
    padding: 7px 16px;
    cursor: pointer;
    box-shadow: 0 4px 16px -3px rgba(168, 85, 247, 0.5);
    animation: assist-shimmer 7s ease infinite;
    transition:
      transform 0.15s,
      box-shadow 0.15s,
      filter 0.15s;
  }

  @keyframes assist-shimmer {
    0%,
    100% {
      background-position: 0% 50%;
    }
    50% {
      background-position: 100% 50%;
    }
  }

  .assist-launch:hover {
    transform: translateY(-1px);
    box-shadow: 0 7px 22px -3px rgba(168, 85, 247, 0.6);
    filter: brightness(1.06);
  }

  .assist-launch.on {
    box-shadow:
      0 0 0 2px color-mix(in srgb, #a855f7 55%, transparent),
      0 4px 16px -3px rgba(168, 85, 247, 0.5);
  }

  .assist-launch .spark {
    font-size: 14px;
    animation: spark-twinkle 2.4s ease-in-out infinite;
  }

  @keyframes spark-twinkle {
    0%,
    100% {
      opacity: 1;
      transform: scale(1);
    }
    50% {
      opacity: 0.72;
      transform: scale(0.9);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .assist-launch,
    .assist-launch .spark {
      animation: none;
    }
  }
</style>
