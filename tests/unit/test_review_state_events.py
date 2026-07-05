import json
from pathlib import Path

from super_dev.review_state import save_workflow_state, workflow_event_log_file


def test_save_workflow_state_appends_event_log(temp_project_dir: Path) -> None:
    save_workflow_state(
        temp_project_dir,
        {
            "status": "waiting_docs_confirmation",
            "workflow_mode": "revise",
            "current_step_label": "等待三文档确认",
            "recommended_command": 'super-dev review docs --status confirmed --comment "三文档已确认"',
        },
    )

    event_log = workflow_event_log_file(temp_project_dir)

    assert event_log.exists()
    lines = [line for line in event_log.read_text(encoding="utf-8").splitlines() if line.strip()]
    assert lines
    payload = json.loads(lines[-1])
    assert payload["event"] == "workflow_state_saved"
    assert payload["status"] == "waiting_docs_confirmation"
    assert payload["current_step_label"] == "等待三文档确认"
