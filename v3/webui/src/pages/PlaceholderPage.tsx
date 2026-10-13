// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { Construction } from "lucide-react";
import { EmptyState } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";

const LATER: Record<string, { subtitle: string; body: string }> = {
  Backups: {
    subtitle: "Backups to the backup appliance",
    body: "Backups arrive with the backup appliance. Until then, this page has nothing to show.",
  },
};

export function PlaceholderPage({ title }: { title: string }) {
  const later = LATER[title];
  return (
    <>
      <PageHeader title={title} subtitle={later?.subtitle} />
      <PageBody>
        <EmptyState icon={<Construction />} title={`${title} isn't built yet`} body={later?.body ?? "Jobs and operations are on the Activity page."} />
      </PageBody>
    </>
  );
}
