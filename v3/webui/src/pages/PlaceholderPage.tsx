// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { Construction } from "lucide-react";
import { EmptyState } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";

export function PlaceholderPage({ title }: { title: string }) {
  return (
    <>
      <PageHeader title={title} />
      <PageBody>
        <EmptyState icon={<Construction />} title={`${title} isn't built yet`} body="Jobs and operations are on the Activity page." />
      </PageBody>
    </>
  );
}
