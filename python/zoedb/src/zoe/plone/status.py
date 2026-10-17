"""The workflow status of an object with the behaviour `zoe.plone.history`
(ADR 0046, section 9).

Products.CMFCore stores a status through the adapter `IWorkflowStatus` of
the object and its workflow: `DefaultWorkflowStatus.set` adds the status to
`workflow_history`, and makes that a `PersistentMapping` when it is
missing. `AppendOnlyWorkflowStatus` is the adapter of an object with the
behaviour: its `set` gives the object a `WorkflowHistory` first, with
`history.ensure`. So a history that a check-in of plone.app.iterate or an
import of a site puts back as a `PersistentMapping` becomes a
`WorkflowHistory` at the next transition, and an object of a type without
a workflow gets no history.

Plone loads this module through `configure.zcml` only.
"""

from Products.CMFCore.interfaces import IWorkflowDefinition, IWorkflowStatus
from zope.component import adapter
from zope.interface import implementer

from zoe.plone.history import IAppendOnlyHistory, ensure


@implementer(IWorkflowStatus)
@adapter(IAppendOnlyHistory, IWorkflowDefinition)
class AppendOnlyWorkflowStatus:
    """The status of an object with the behaviour `zoe.plone.history` in a
    workflow."""

    def __init__(self, context, workflow):
        self.context = getattr(context, "aq_base", context)
        self.wf_id = workflow.getId()

    def get(self):
        history = getattr(self.context, "workflow_history", {})
        statuses = history.get(self.wf_id)
        if statuses:
            return statuses[-1]
        return None

    def set(self, status):
        ensure(self.context)
        history = self.context.workflow_history
        statuses = list(history.get(self.wf_id, ()))
        statuses.append(status)
        history[self.wf_id] = tuple(statuses)
