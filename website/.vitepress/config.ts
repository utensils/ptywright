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
    ['meta', { name: 'theme-color', content: '#0891B2' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:title', content: 'ptywright' }],
    ['meta', { property: 'og:description', content: description }],
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
        text: 'v0.1.0',
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
            { text: 'Design principles', link: '/guide/design-principles' },
            { text: 'Platforms', link: '/guide/platforms' },
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
