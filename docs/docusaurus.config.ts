import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

// Every locale — English included — is served under its own path segment
// (/momotaro/en, /momotaro/ja, /momotaro/zh-CN), so no language sits at the
// site root. The root itself only carries a meta-refresh hop to the default
// locale, written after the build by scripts/write-root-redirect.mjs.
const config: Config = {
  title: 'Momotaro',
  tagline: 'A local-first knowledge engine for serious STEM learners.',
  favicon: 'img/favicon.ico',

  future: {
    v4: true,
  },

  url: 'https://guangyiliushan.github.io',
  baseUrl: '/momotaro/',

  organizationName: 'guangyiliushan',
  projectName: 'momotaro',

  onBrokenLinks: 'throw',
  markdown: {
    mermaid: true,
    hooks: {
      onBrokenMarkdownLinks: 'throw',
    },
  },

  i18n: {
    defaultLocale: 'en',
    locales: ['en', 'ja', 'zh-CN'],
    localeConfigs: {
      en: {
        label: 'English',
        baseUrl: '/momotaro/en/',
      },
      ja: {
        label: '日本語',
        baseUrl: '/momotaro/ja/',
      },
      'zh-CN': {
        label: '简体中文',
        baseUrl: '/momotaro/zh-CN/',
      },
    },
  },

  presets: [
    [
      'classic',
      {
        docs: {
          routeBasePath: '/',
          sidebarPath: './sidebars.ts',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themes: [
    '@docusaurus/theme-mermaid',
    [
      '@easyops-cn/docusaurus-search-local',
      {
        hashed: true,
        docsRouteBasePath: '/',
        indexDocs: true,
        indexBlog: false,
        indexPages: false,
        language: ['en', 'ja', 'zh'],
      },
    ],
  ],

  plugins: [
    [
      'docusaurus-plugin-llms',
      {
        generateLLMsTxt: true,
        generateLLMsFullTxt: true,
        generateMarkdownFiles: true,
        excludeImports: true,
        removeDuplicateHeadings: true,
      },
    ],
  ],

  themeConfig: {
    colorMode: {
      respectPrefersColorScheme: true,
    },
    navbar: {
      title: 'Momotaro',
      items: [
        {
          type: 'docSidebar',
          sidebarId: 'docs',
          position: 'left',
          label: 'Docs',
        },
        {
          type: 'localeDropdown',
          position: 'right',
        },
        {
          href: 'https://github.com/guangyiliushan/momotaro',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      copyright: `Copyright &copy; ${new Date().getFullYear()} Momotaro contributors.`,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
