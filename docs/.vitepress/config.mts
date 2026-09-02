import { defineConfig } from "vitepress";

export default defineConfig({
  title: "onelf",
  description:
    "Single-binary packaging for Linux. One file that runs from a private mount, takes GPU drivers from the host on purpose, and leaves nothing behind.",
  cleanUrls: true,
  lastUpdated: true,
  appearance: "dark",

  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: "/logo.svg" }],
    ["link", { rel: "preconnect", href: "https://fonts.googleapis.com" }],
    [
      "link",
      { rel: "preconnect", href: "https://fonts.gstatic.com", crossorigin: "" },
    ],
    [
      "link",
      {
        rel: "stylesheet",
        href: "https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@500;600;700&family=JetBrains+Mono:wght@400;500;600&display=swap",
      },
    ],
    ["meta", { name: "theme-color", content: "#0f0e0c" }],
  ],

  themeConfig: {
    logo: { src: "/logo.svg", width: 24, height: 24 },

    nav: [
      { text: "Guide", link: "/guide/introduction", activeMatch: "/guide/" },
      { text: "Reference", link: "/reference/cli", activeMatch: "/reference/" },
      { text: "Concepts", link: "/guide/concepts" },
    ],

    sidebar: {
      "/guide/": [
        {
          text: "Getting Started",
          items: [
            { text: "Introduction", link: "/guide/introduction" },
            { text: "How onelf Thinks", link: "/guide/concepts" },
            { text: "Installation", link: "/guide/installation" },
            { text: "Quick Start", link: "/guide/quick-start" },
          ],
        },
        {
          text: "Packaging",
          items: [
            { text: "AppDir Layout", link: "/guide/appdir-layout" },
            { text: "Bundling Libraries", link: "/guide/bundling" },
            { text: "Bundling from a Sysroot", link: "/guide/sysroot" },
            { text: "Cross-libc Packages", link: "/guide/cross-libc" },
            { text: "Recipe File", link: "/guide/recipe" },
            { text: "Reproducible Builds", link: "/guide/reproducible" },
          ],
        },
        {
          text: "Runtime",
          items: [
            { text: "Execution Modes", link: "/guide/execution-modes" },
            { text: "Entrypoints", link: "/guide/entrypoints" },
            { text: "Environment", link: "/guide/environment" },
            { text: "Portable Directories", link: "/guide/portable-dirs" },
          ],
        },
        {
          text: "Distribution",
          items: [
            { text: "Self-Update", link: "/guide/self-update" },
            { text: "Desktop Integration", link: "/guide/desktop" },
            { text: "Integrity Verification", link: "/guide/verify" },
          ],
        },
        {
          text: "Development",
          items: [
            { text: "Developing Packages", link: "/guide/developing" },
            { text: "Inspecting Packages", link: "/guide/inspecting" },
          ],
        },
        {
          text: "Examples",
          items: [
            { text: "Miniflux + PostgreSQL", link: "/guide/examples/miniflux" },
          ],
        },
      ],
      "/reference/": [
        {
          text: "Reference",
          items: [
            { text: "CLI", link: "/reference/cli" },
            { text: "Recipe Schema", link: "/reference/recipe-schema" },
            { text: "Environment Variables", link: "/reference/env-vars" },
            { text: "Runtime Flags", link: "/reference/runtime-flags" },
            { text: "File Format", link: "/reference/file-format" },
          ],
        },
      ],
    },

    outline: { level: [2, 3] },

    socialLinks: [
      { icon: "github", link: "https://github.com/QaidVoid/onelf" },
    ],

    editLink: {
      pattern: "https://github.com/QaidVoid/onelf/edit/main/docs/:path",
      text: "Edit this page",
    },

    footer: {
      message: "Released under the MIT License.",
      copyright: "Copyright QaidVoid",
    },

    search: {
      provider: "local",
    },
  },
});
