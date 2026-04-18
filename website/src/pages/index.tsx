import type {ReactNode} from 'react';
import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';

import styles from './index.module.css';

function HomepageHeader() {
  const {siteConfig} = useDocusaurusContext();
  return (
    <header className={clsx('hero hero--primary', styles.heroBanner)}>
      <div className="container">
        <Heading as="h1" className="hero__title">
          {siteConfig.title}
        </Heading>
        <p className="hero__subtitle">{siteConfig.tagline}</p>
        <p className={styles.heroByline}>
          Persistent memory for AI agents. Local-first. Zero cloud. Apache-2.0.
        </p>
        <div className={styles.buttons}>
          <Link
            className="button button--secondary button--lg"
            to="/docs/user/quickstart">
            Quickstart →
          </Link>
          <Link
            className="button button--outline button--secondary button--lg"
            to="/docs/">
            What is ai-memory?
          </Link>
          <Link
            className="button button--outline button--secondary button--lg"
            href="https://github.com/alphaonedev/ai-memory-mcp">
            GitHub
          </Link>
        </div>
      </div>
    </header>
  );
}

function HomepageFeatures() {
  return (
    <section className="container" style={{paddingTop: '3rem', paddingBottom: '3rem'}}>
      <div className="feature-grid">
        <div className="feature-card">
          <h3>Local-first</h3>
          <p>
            One SQLite file. No cloud, no login, no SaaS. Your memories live on your hardware
            and survive vendor outages, model deprecations, and infrastructure rebuilds.
          </p>
        </div>
        <div className="feature-card">
          <h3>Peer-to-peer mesh</h3>
          <p>
            <code>ai-memory sync-daemon --peers https://laptop-b:9077</code> forms a live
            knowledge mesh between any two ai-memory instances. One agent learns it, every
            peer knows it within a cycle.
          </p>
        </div>
        <div className="feature-card">
          <h3>Three tiers, four feature levels</h3>
          <p>
            Memory tiers: short / mid / long. Feature tiers: keyword → semantic → smart
            (Gemma 4 E2B) → autonomous (Gemma 4 E4B). Start free, scale as needed.
          </p>
        </div>
        <div className="feature-card">
          <h3>MCP-native</h3>
          <p>
            Works with Claude Code, ChatGPT, OpenAI Codex, xAI Grok, Cursor, OpenClaw, and
            any MCP-compatible client out of the box.
          </p>
        </div>
        <div className="feature-card">
          <h3>Hardened crypto</h3>
          <p>
            Native TLS for HTTPS sync. mTLS with SHA-256 fingerprint pinning rejects unknown
            peers at the TLS handshake — known_hosts-style trust, no CA required.
          </p>
        </div>
        <div className="feature-card">
          <h3>Hybrid recall</h3>
          <p>
            FTS5 keyword + cosine similarity + memory-link graph traversal, blended in a
            6-factor score. <strong>97.8% Recall@5</strong> on LongMemEval-S, all-local.
          </p>
        </div>
      </div>
    </section>
  );
}

export default function Home(): ReactNode {
  const {siteConfig} = useDocusaurusContext();
  return (
    <Layout
      title={`${siteConfig.title} — AI endpoint memory`}
      description="Persistent, peer-synced memory for AI agents. Local-first. Apache-2.0.">
      <HomepageHeader />
      <main>
        <HomepageFeatures />
      </main>
    </Layout>
  );
}
