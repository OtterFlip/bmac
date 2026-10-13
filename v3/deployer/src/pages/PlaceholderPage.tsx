// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import type { ReactNode } from "react";
import { EmptyState } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/Page";

export function PlaceholderPage({ title, subtitle, icon }: { title: string; subtitle: string; icon: ReactNode }) {
  return (
    <>
      <PageHeader title={title} subtitle={subtitle} />
      <PageBody>
        <EmptyState icon={icon} title="Not built yet" body="This part of the Deployer arrives in a later step of milestone 6." />
      </PageBody>
    </>
  );
}
