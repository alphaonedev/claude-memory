import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

const config: Config = {
  title: 'ai-memory',
  tagline: 'AI endpoint memory — a primitive, not a product',
  favicon: 'img/favicon.ico',

  future: {
    v4: true,
  },

  url: 'https://alphaonedev.github.io',
  baseUrl: '/ai-memory-mcp/',

  organizationName: 'alphaonedev',
  projectName: 'ai-memory-mcp',
  deploymentBranch: 'gh-pages',
  trailingSlash: false,

  onBrokenLinks: 'warn',

  markdown: {
    hooks: {
      onBrokenMarkdownLinks: 'warn',
    },
  },

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      {
        docs: {
          sidebarPath: './sidebars.ts',
          editUrl:
            'https://github.com/alphaonedev/ai-memory-mcp/tree/main/website/',
          showLastUpdateTime: true,
          showLastUpdateAuthor: true,
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    image: 'img/ai-memory-logo.jpg',
    colorMode: {
      defaultMode: 'dark',
      disableSwitch: false,
      respectPrefersColorScheme: true,
    },
    navbar: {
      title: 'ai-memory',
      logo: {
        alt: 'ai-memory',
        src: 'img/ai-memory-logo.jpg',
      },
      items: [
        {to: '/docs/user/quickstart', label: 'User', position: 'left'},
        {to: '/docs/admin/deployment', label: 'Admin', position: 'left'},
        {to: '/docs/developer/architecture', label: 'Developer', position: 'left'},
        {
          type: 'dropdown',
          label: 'Architectures',
          position: 'left',
          items: [
            {label: 'Overview & comparison', to: '/docs/architectures/'},
            {label: 'T1 — Single node, single agent', to: '/docs/architectures/t1-single-node-single-agent'},
            {label: 'T2 — Single node, many agents', to: '/docs/architectures/t2-single-node-many-agents'},
            {label: 'T3 — Multi-node cluster', to: '/docs/architectures/t3-multi-node-cluster'},
            {label: 'T4 — Data-center swarm', to: '/docs/architectures/t4-data-center-swarm'},
            {label: 'T5 — Global hive', to: '/docs/architectures/t5-global-hive'},
          ],
        },
        {to: '/docs/changelog', label: 'Changelog', position: 'left'},
        {
          href: 'https://github.com/alphaonedev/ai-memory-mcp',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      links: [
        {
          title: 'Docs',
          items: [
            {label: 'Quickstart', to: '/docs/user/quickstart'},
            {label: 'Install', to: '/docs/user/install'},
            {label: 'Tiers', to: '/docs/user/tiers'},
          ],
        },
        {
          title: 'Operate',
          items: [
            {label: 'Deployment', to: '/docs/admin/deployment'},
            {label: 'TLS / mTLS', to: '/docs/admin/tls-mtls'},
            {label: 'Peer mesh', to: '/docs/admin/peer-mesh'},
          ],
        },
        {
          title: 'Build',
          items: [
            {label: 'Architecture', to: '/docs/developer/architecture'},
            {label: 'MCP tools', to: '/docs/developer/mcp-tools'},
            {label: 'HTTP API', to: '/docs/developer/http-api'},
          ],
        },
        {
          title: 'More',
          items: [
            {label: 'GitHub', href: 'https://github.com/alphaonedev/ai-memory-mcp'},
            {label: 'Issues', href: 'https://github.com/alphaonedev/ai-memory-mcp/issues'},
            {label: 'Releases', href: 'https://github.com/alphaonedev/ai-memory-mcp/releases'},
          ],
        },
      ],
      copyright: `Copyright © ${new Date().getFullYear()} AlphaOne LLC. Apache-2.0 licensed. ai-memory™ is a trademark of AlphaOne LLC (USPTO Serial 99761257).`,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
      additionalLanguages: ['rust', 'toml', 'bash', 'json', 'sql', 'yaml'],
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
