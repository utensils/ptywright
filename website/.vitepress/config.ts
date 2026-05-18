import { defineConfig } from 'vitepress'
import tailwindcss from '@tailwindcss/vite'

const description =
  'A cross-platform Rust CLI and library for driving interactive terminal applications through PTYs.'

export default defineConfig({
  title: 'ptywright',
  description,
  base: '/ptywright/',

  vite: {
    plugins: [tailwindcss()],
    server: {
      allowedHosts: true,
    },
  },

  head: [
    [
      'link',
      { rel: 'icon', href: '/ptywright/favicon.svg', type: 'image/svg+xml' },
    ],
    // Two theme-color tags so mobile browser UI tracks the active mode.
    [
      'meta',
      {
        name: 'theme-color',
        content: '#f4f5f3',
        media: '(prefers-color-scheme: light)',
      },
    ],
    [
      'meta',
      {
        name: 'theme-color',
        content: '#0a0b0d',
        media: '(prefers-color-scheme: dark)',
      },
    ],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:title', content: 'ptywright' }],
    ['meta', { property: 'og:description', content: description }],
    ['link', { rel: 'preconnect', href: 'https://fonts.googleapis.com' }],
    [
      'link',
      {
        rel: 'preconnect',
        href: 'https://fonts.gstatic.com',
        crossorigin: '',
      },
    ],
    [
      'link',
      {
        rel: 'stylesheet',
        href: 'https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;600;700&family=IBM+Plex+Mono:wght@400;500;600&display=swap',
      },
    ],
  ],

  lastUpdated: true,

  markdown: {
    theme: {
      light: 'catppuccin-latte',
      dark: 'catppuccin-mocha',
    },
  },

  sitemap: {
    hostname: 'https://utensils.io/ptywright/',
  },

  themeConfig: {
    logo: '/favicon.svg',
    siteTitle: 'ptywright',

    nav: [
      { text: 'Guide', link: '/guide/' },
      { text: 'Reference', link: '/reference/' },
      {
        text: 'v0.1.1',
        items: [
          {
            text: 'Changelog',
            link: 'https://github.com/utensils/ptywright/releases',
          },
          {
            text: 'Cargo.toml',
            link: 'https://github.com/utensils/ptywright/blob/main/Cargo.toml',
          },
        ],
      },
    ],

    sidebar: {
      '/guide/': [
        {
          text: 'Getting Started',
          items: [
            { text: 'What is ptywright?', link: '/guide/' },
            { text: 'Installation', link: '/guide/installation' },
            { text: 'Quickstart', link: '/guide/quickstart' },
          ],
        },
        {
          text: 'Design',
          items: [
            { text: 'Architecture', link: '/guide/architecture' },
            { text: 'Extensions', link: '/guide/extensions' },
            { text: 'Claude Code adapter', link: '/guide/claude-code' },
            { text: 'Design principles', link: '/guide/design-principles' },
            { text: 'Platforms', link: '/guide/platforms' },
            { text: 'Runtime directory', link: '/guide/runtime-directory' },
          ],
        },
      ],
      '/reference/': [
        {
          text: 'Reference',
          items: [
            { text: 'Overview', link: '/reference/' },
            { text: 'CLI', link: '/reference/cli' },
            { text: 'Library', link: '/reference/library' },
            { text: 'JSON-RPC', link: '/reference/json-rpc' },
            { text: 'Plugins', link: '/reference/plugins' },
            { text: 'Roadmap', link: '/reference/roadmap' },
          ],
        },
      ],
    },

    socialLinks: [
      { icon: 'github', link: 'https://github.com/utensils/ptywright' },
    ],

    search: {
      provider: 'local',
    },

    footer: {
      message: 'Released under the MIT License.',
      copyright:
        'Copyright © <a href="https://jamesbrink.online/">James Brink</a>',
    },

    editLink: {
      pattern: 'https://github.com/utensils/ptywright/edit/main/website/:path',
      text: 'Edit this page on GitHub',
    },

    outline: {
      level: [2, 3],
    },
  },
})
