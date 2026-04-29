import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

const sidebars: SidebarsConfig = {
  userSidebar: [
    {
      type: 'category',
      label: 'Getting started',
      collapsed: false,
      items: ['intro', 'user/install', 'user/quickstart', 'user/import-history'],
    },
    {
      type: 'category',
      label: 'Concepts',
      collapsed: false,
      items: ['user/tiers', 'user/recall', 'user/namespaces', 'user/agent-identity'],
    },
    {
      type: 'category',
      label: 'Workflows',
      items: ['user/workflows', 'user/troubleshooting'],
    },
  ],
  adminSidebar: [
    {
      type: 'category',
      label: 'Operate',
      collapsed: false,
      items: ['admin/deployment', 'admin/upgrade', 'admin/backup'],
    },
    {
      type: 'category',
      label: 'Security',
      collapsed: false,
      items: ['admin/tls-mtls', 'admin/peer-mesh', 'admin/security'],
    },
    {
      type: 'category',
      label: 'Governance',
      items: ['admin/governance', 'admin/observability'],
    },
  ],
  developerSidebar: [
    {
      type: 'category',
      label: 'Architecture',
      collapsed: false,
      items: ['developer/architecture', 'developer/data-model', 'developer/recall-pipeline'],
    },
    {
      type: 'category',
      label: 'Reference',
      collapsed: false,
      items: ['developer/mcp-tools', 'developer/http-api', 'developer/cli-reference'],
    },
    {
      type: 'category',
      label: 'Contribute',
      items: ['developer/building', 'developer/contributing', 'developer/governance-model'],
    },
  ],
  changelogSidebar: ['changelog'],
  architecturesSidebar: [
    {
      type: 'category',
      label: 'Architectures',
      collapsed: false,
      items: [
        'architectures/index',
        'architectures/t1-single-node-single-agent',
        'architectures/t2-single-node-many-agents',
        'architectures/t3-multi-node-cluster',
        'architectures/t4-data-center-swarm',
        'architectures/t5-global-hive',
      ],
    },
  ],
};

export default sidebars;
