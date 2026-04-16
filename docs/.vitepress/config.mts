import { defineConfig } from "vitepress";

export default defineConfig({
  title: "onelf",
  description: "Single-binary packaging for Linux. Pack any app into one file.",
  cleanUrls: true,
  lastUpdated: true,

  themeConfig: {
    nav: [
      { text: "Guide", link: "/guide/introduction" },
      { text: "Reference", link: "/reference/cli" },
      { text: "GitHub", link: "https://github.com/QaidVoid/onelf" },
    ],

    sidebar: {
      "/guide/": [
        {
          text: "Getting Started",
          items: [
            { text: "Introduction", link: "/guide/introduction" },
            { text: "Installation", link: "/guide/installation" },
            { text: "Quick Start", link: "/guide/quick-start" },
          ],
        },
        {
          text: "Packaging",
          items: [
            { text: "AppDir Layout", link: "/guide/appdir-layout" },
            { text: "Bundling Libraries", link: "/guide/bundling" },
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

    socialLinks: [
      { icon: "github", link: "https://github.com/QaidVoid/onelf" },
    ],

    footer: {
      message: "Released under the MIT License.",
      copyright: "Copyright © QaidVoid",
    },

    search: {
      provider: "local",
    },
  },
});
